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
#
# cargo-chef splits the Rust build into a dependency-only "cook" layer (keyed on
# recipe.json — the dependency graph alone) and a thin app-compile layer. This
# matters for CI caching: a BuildKit `--mount=type=cache` target dir is LOCAL to
# the builder and is NOT exported by `cache-to: type=gha`, so on a fresh CI
# runner it starts empty and every build recompiled the whole dependency tree
# from scratch (~8 min). The cook layer, by contrast, is a real image layer that
# GHA layer caching persists across runs. A normal commit changes app crates but
# not the dependency graph, so the cooked deps are reused and only patom's own
# crates recompile.
#
# All SQL goes through runtime sqlx (bound params + FromRow), so the build needs
# no database — there are no compile-time query macros to verify against a schema.
FROM rust:1-bookworm AS chef
WORKDIR /app
ENV CARGO_TERM_COLOR=never
# cargo-chef is a build tool, not a runtime dep; it lives only in this builder
# image. Installed on the image's default toolchain (chef is toolchain-agnostic);
# the pinned rust-toolchain.toml channel governs the actual cook/build below.
RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
# recipe.json captures only the dependency graph, not source — editing app code
# does not change it, so the cook layer below stays cached across commits.
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Cargo features to enable, space-separated. Empty (the default) builds the
# OSS / self-host binary that links no cloud code — this is what the public
# `ghcr.io/tomkapa/patom` image ships. The SaaS image is built with
# `FEATURES=cloud`, which pulls in `patom-cloud` and flips `AppState.cloud`
# (cfg!(feature = "cloud")) on, enabling self-service workspace creation
# (`POST /me/orgs`). Keep this empty here so a plain `docker build` stays OSS.
ARG FEATURES=""
# Pinned toolchain must govern the cook so cooked artifacts match the final
# build's fingerprints (otherwise cargo recompiles the deps anyway).
COPY rust-toolchain.toml ./
COPY --from=planner /app/recipe.json recipe.json
# Compile dependencies only. `--features` is package-scoped in a virtual
# workspace, so scope to the binary's package (`-p patom-server`); the flag is
# appended only when FEATURES is non-empty. Cached until recipe.json changes.
RUN cargo chef cook --release --recipe-path recipe.json \
      -p patom-server ${FEATURES:+--features "${FEATURES}"}
# Now copy real source and compile only patom's crates against the cooked deps.
# Workspace layout (issue #133): all crates live under crates/. patom-core's
# `migrations/` is embedded at COMPILE time by sqlx::migrate!("./migrations"), so
# the runtime image omits it. tests/ are excluded via .dockerignore, so editing a
# test does not bust this layer.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p patom-server --bin patom \
      ${FEATURES:+--features "${FEATURES}"} \
 && cp target/release/patom /usr/local/bin/patom

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
