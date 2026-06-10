# syntax=docker/dockerfile:1.7
#
# Multi-stage build for patom. Built in CI, never on the prod node (the linker
# OOMs at 4 GB). Three stages: bun SPA → rust release binary → distroless
# runtime. See deploy plan / CLAUDE.md.

###########################  Stage 1: web SPA  ###########################
# glibc bun image (oven/bun:1-debian) avoids esbuild musl edge cases.
FROM oven/bun:1-debian AS web
WORKDIR /web
# Install against the committed lockfile first so this layer caches on deps alone.
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile
# web/node_modules + web/dist are .dockerignore'd, so this copies source only.
COPY web/ ./
RUN bun run build                       # build.ts -> /web/dist

###########################  Stage 2: rust build  #######################
# rustup-based image so rust-toolchain.toml's pinned channel is honored.
FROM rust:1-bookworm AS builder
WORKDIR /app
# All SQL goes through runtime sqlx (bound params + FromRow), so the build needs
# no database — there are no compile-time query macros to verify against a schema.
ENV CARGO_TERM_COLOR=never
# Dependency + build caching is handled by the BuildKit cache mounts below
# (registry + target). A stub "warm-up" layer would be masked by the /app/target
# cache mount, so it's omitted — the mounts make incremental CI builds fast.
# Workspace layout (issue #133): all crates live under crates/. patom-core's
# `migrations/` is embedded at COMPILE time by sqlx::migrate!("./migrations"), so
# the runtime image omits it. tests/ are excluded via .dockerignore, so editing a
# test does not bust this layer.
# Cargo features to enable, space-separated. Empty (the default) builds the
# OSS / self-host binary that links no cloud code — this is what the public
# `ghcr.io/tomkapa/patom` image ships. The SaaS image is built with
# `FEATURES=cloud`, which pulls in `patom-cloud` and flips `AppState.cloud`
# (cfg!(feature = "cloud")) on, enabling self-service workspace creation
# (`POST /me/orgs`). Keep this empty here so a plain `docker build` stays OSS.
ARG FEATURES=""
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
# `--features` is package-scoped in a virtual workspace, so always select the
# binary's package (`-p patom-server`); the feature flag is appended only when
# FEATURES is non-empty. The target cache is keyed on FEATURES so the OSS and
# cloud variants don't evict each other's incremental artifacts.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target,id=patom-target-${FEATURES} \
    cargo build --release --locked -p patom-server --bin patom \
      ${FEATURES:+--features "${FEATURES}"} \
 && cp /app/target/release/patom /usr/local/bin/patom

###########################  Stage 3: runtime  ##########################
# distroless/cc: glibc + libgcc + ca-certificates, no shell/pkg-manager. Works
# because every networked dep is rustls (no OpenSSL); CA certs cover outbound
# TLS to the LLM/embedding/search/R2 APIs.
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /usr/local/bin/patom /usr/local/bin/patom
COPY --from=web    /web/dist                ./web/dist
ENV HTTP_ADDR=0.0.0.0:8080 \
    PATOM_WEB_DIST=/app/web/dist
EXPOSE 8080
USER nonroot:nonroot                    # uid 65532
ENTRYPOINT ["/usr/local/bin/patom"]
