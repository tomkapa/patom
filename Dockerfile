# syntax=docker/dockerfile:1.7
#
# Multi-stage build for patom-rs. Built in CI, never on the prod node (the linker
# OOMs at 4 GB). Three stages: bun SPA → rust release binary (SQLX_OFFLINE) →
# distroless runtime. See deploy plan / CLAUDE.md.

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
# SQLX_OFFLINE: compile-time query!/query_as! macros check against the committed
# .sqlx cache instead of a live DB, so the image build never needs Postgres.
ENV CARGO_TERM_COLOR=never \
    SQLX_OFFLINE=true
# Dependency pre-cache: build all deps against a stub main so the heavy layer
# caches independently of source churn (matters for CI layer caching).
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked --bin patom-rs || true
RUN rm -rf src
# Real sources. `.sqlx/` is the committed offline query-macro cache (see ENV note).
# `migrations/` is embedded at COMPILE time by sqlx::migrate!("./migrations"), so the
# runtime image omits it.
COPY .sqlx ./.sqlx
COPY migrations ./migrations
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --bin patom-rs \
 && cp /app/target/release/patom-rs /usr/local/bin/patom-rs

###########################  Stage 3: runtime  ##########################
# distroless/cc: glibc + libgcc + ca-certificates, no shell/pkg-manager. Works
# because every networked dep is rustls (no OpenSSL); CA certs cover outbound
# TLS to the LLM/embedding/search/R2 APIs.
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /usr/local/bin/patom-rs /usr/local/bin/patom-rs
COPY --from=web    /web/dist                ./web/dist
ENV HTTP_ADDR=0.0.0.0:8080 \
    PATOM_WEB_DIST=/app/web/dist
EXPOSE 8080
USER nonroot:nonroot                    # uid 65532
ENTRYPOINT ["/usr/local/bin/patom-rs"]
