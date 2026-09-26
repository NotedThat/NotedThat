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
#
# ==============================================================================

# ------------------------------------------------------------------------------
# Stage 1: chef — Rust toolchain + cargo-chef, pinned by digest.
#
# The digest is the reference this build resolves; the tag beside it is there so
# a human can tell at a glance what the digest is supposed to be, and is NOT
# what Docker looks up. Renovate's docker manager keeps both halves current and
# raises the bump as an ordinary PR, so a base-image change is a reviewable
# commit rather than something that happens silently between two builds of the
# same source (§ Supply Chain Notes in RELEASING.md).
#
# Consequence worth knowing: this also pins the compiler the image builds with
# (currently 1.98.1), which no longer tracks `dtolnay/rust-toolchain@stable` in
# CI the way the floating tag used to. That gap is covered from the other side
# — `rust-version` in the workspace manifest states the floor the crates
# support, and CI's `test` job runs it alongside stable.
#
# Edition 2024 requires rustc >= 1.85.
# ------------------------------------------------------------------------------
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm@sha256:5a37e174ceb7ccbebd6d0024049e9308f4de399bbcabbc3c6c649c70486880d5 AS chef
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
RUN cargo chef cook --release --recipe-path recipe.json \
    --bin notedthat-server

# Copy the actual sources and compile the distributable binary against the cooked deps.
COPY . .
# The same revision the image is labelled with, baked in so `notedthat_build_info`
# can report it (D69). Declared here as well as in the runtime stage because a
# build argument is scoped to the stage that declares it. Left `unknown` for a
# plain `docker build`, which is honest: nothing told this build what it is.
ARG IMAGE_REVISION=unknown
ENV NOTEDTHAT_BUILD_REVISION=${IMAGE_REVISION}
RUN cargo build --release --locked \
    --bin notedthat-server \
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
#
# Pinned by digest for the same reason as the chef stage above. Note what the
# pin does and does not buy: it fixes the starting layer, not the packages
# `apt-get install` puts on top of it, which still resolve against whatever the
# Debian mirror serves at build time. So this gives stable, traceable inputs
# rather than a bit-reproducible image.
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime

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

# Default storage root for NOTEDTHAT_STORAGE_BACKEND=fs, owned by the runtime user so a
# named volume mounted here is writable without any host-side chown. A bind mount still
# has to be writable by uid 10001.
RUN install -d -o 10001 -g 10001 -m 0755 /var/lib/notedthat

# Mount point for the upload/index staging directory. Nothing is written here
# unless NOTEDTHAT_UPLOAD_TMP_DIR points at it; the default staging directory
# is still the platform temp dir, so behaviour is unchanged for every existing
# caller. It exists because Docker seeds a fresh named volume with the image's
# ownership only when the path already exists in the image — mount a volume at
# a path the image lacks and it arrives root-owned, uid 10001 cannot write, and
# startup refuses. That matters under `--read-only`, where the default /tmp is
# not writable: see "A read-only root filesystem" in docs/OPERATIONS.md.
RUN install -d -o 10001 -g 10001 -m 0700 /var/lib/notedthat-staging

COPY --from=builder /app/target/release/notedthat-server /usr/local/bin/notedthat-server

USER notedthat:notedthat
EXPOSE 8080

# /healthz is an unauthenticated liveness probe served by notedthat-api-http.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8080/healthz || exit 1

# ------------------------------------------------------------------------------
# OCI image metadata.
#
# IMAGE_VERSION and IMAGE_REVISION are injected by the release workflow (see
# .github/workflows/docker.yml). Local `docker build` without --build-arg
# produces "0.1.0" / "unknown", which is fine for dev images.
# ------------------------------------------------------------------------------
ARG IMAGE_VERSION=0.1.0
ARG IMAGE_REVISION=unknown

LABEL org.opencontainers.image.title="notedthat-server" \
      org.opencontainers.image.description="NotedThat markdown-first knowledgebase server (HTTP API + WebDAV + remote MCP)" \
      org.opencontainers.image.source="https://github.com/NotedThat/NotedThat" \
      org.opencontainers.image.url="https://github.com/NotedThat/NotedThat" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.revision="${IMAGE_REVISION}" \
      org.opencontainers.image.licenses="MPL-2.0"

ENTRYPOINT ["/usr/local/bin/notedthat-server"]
