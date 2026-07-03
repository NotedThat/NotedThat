# syntax=docker/dockerfile:1.7
# ==============================================================================
# NotedThat — production image for the `notedthat-server` binary.
#
# Layout:
#   Stage 1 (chef)    — base image with cargo-chef preinstalled.
#   Stage 2 (planner) — compute a dependency-only build recipe (recipe.json).
#   Stage 3 (builder) — cook deps (cached), then compile the server binary.
#   Stage 4 (runtime) — minimal Debian slim + CA certs + curl for HEALTHCHECK.
#
# Build:
#   docker build -t notedthat-server:local .
#
# Run:
#   docker run --rm -p 8080:8080 \
#     -e NOTEDTHAT_API_TOKEN=... -e NOTEDTHAT_KBS=notes,scratch \
#     -e NOTEDTHAT_S3_REGION=us-east-1 \
#     -e NOTEDTHAT_S3_ACCESS_KEY_ID=... -e NOTEDTHAT_S3_SECRET_ACCESS_KEY=... \
#     notedthat-server:local
# ==============================================================================

# ------------------------------------------------------------------------------
# Stage 1: chef — Rust toolchain + cargo-chef, pinned via the official image.
# `latest-rust-1-bookworm` tracks the latest 1.x stable release, matching CI
# (`dtolnay/rust-toolchain@stable`). Edition 2024 requires rustc >= 1.85.
# ------------------------------------------------------------------------------
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

# ------------------------------------------------------------------------------
# Stage 2: planner — inspect Cargo.toml/Cargo.lock and emit recipe.json.
# The full source tree is copied here, but only recipe.json flows into the
# builder stage, so source changes do NOT bust the dependency cache below.
# ------------------------------------------------------------------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ------------------------------------------------------------------------------
# Stage 3: builder — cook dependencies first (cached), then build the binary.
# ------------------------------------------------------------------------------
FROM chef AS builder

# Cook only the dependencies. This layer is cached until Cargo.lock or any
# workspace Cargo.toml changes — source edits do not invalidate it.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json --bin notedthat-server

# Copy the actual sources and compile the server binary against the cooked deps.
COPY . .
RUN cargo build --release --locked --bin notedthat-server \
 && strip target/release/notedthat-server

# ------------------------------------------------------------------------------
# Stage 4: runtime — small Debian slim with the binary and just enough tooling.
#
# Why debian-slim (not distroless / alpine):
#   * distroless has no `curl`, breaking the HEALTHCHECK below.
#   * alpine (musl) would require rebuilding with a musl target, adding
#     toolchain complexity for marginal size savings.
#
# The Rust binary handles SIGTERM/SIGINT itself via tokio::signal, so no
# tini/dumb-init init wrapper is needed — the server IS PID 1.
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install --yes --no-install-recommends \
        ca-certificates \
        curl \
 && rm -rf /var/lib/apt/lists/*

# Dedicated non-root user. Fixed UID/GID so any bind-mounted volumes have
# predictable ownership across hosts.
RUN groupadd --system --gid 10001 notedthat \
 && useradd  --system --uid 10001 --gid notedthat \
        --home-dir /nonexistent --shell /usr/sbin/nologin notedthat

COPY --from=builder /app/target/release/notedthat-server /usr/local/bin/notedthat-server

USER notedthat:notedthat
EXPOSE 8080
EXPOSE 8081

# /healthz is an unauthenticated liveness probe served by notedthat-api-http.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/notedthat-server"]
