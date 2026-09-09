# Development Guide

## Prerequisites

- Rust stable (edition 2024 requires rustc 1.85+)
- Install via [rustup](https://rustup.rs/): `rustup default stable`
- No `rust-toolchain.toml` is present — the project tracks stable Rust directly, matching the `dtolnay/rust-toolchain@stable` pin used in CI.

## Daily Commands

```sh
# Fast type check (no codegen)
cargo check --workspace

# Run all tests
cargo test --workspace --locked

# Lint (mirrors CI)
cargo clippy --workspace --all-targets --locked -- -D warnings

# Format check (mirrors CI)
cargo fmt --all -- --check

# Format in place
cargo fmt --all
```

## Local service setup

The supported development paths are documented in the root [README](README.md):

- **Compose** builds the server and runs SeaweedFS and Qdrant, while you supply an
  OpenAI-compatible embedding provider in an ignored local `.env` file.
- **Native server** uses Compose-managed SeaweedFS and Qdrant with host-facing
  endpoints, then runs `cargo run --bin notedthat-server`.

Do not commit provider credentials. Copy `.env.example` to `.env`, replace its
embedding endpoint, model, key, and dimensions, then follow the chosen README flow.

## Running a Specific Crate

```sh
cargo run --bin notedthat-mcp-stdio
```

Both binaries are owned by the `notedthat` crate, so select them with `--bin`;
`-p notedthat-server` and `-p notedthat-mcp-stdio` name library crates and have
nothing to run.

## Installing the MCP stdio wrapper

`notedthat-mcp-stdio` is the subprocess an MCP client (opencode, Claude Desktop, Cursor, Zed) spawns to talk to the server. To keep an MCP client wired up against a local build, install the binary somewhere on `PATH`. The Makefile has two paths, both writing to `$HOME/.local/bin/notedthat-mcp-stdio`:

```sh
make mcp-stdio             # cargo install from source — reflects your working tree
                           # (cargo install --path crates/notedthat installs both binaries)
make mcp-stdio-from-image  # docker cp from notedthat-server:local — fast, requires a built image
```

Override `PREFIX=/some/dir` or `IMAGE=some:tag` on the command line to install elsewhere. See `make help` for the full listing.

## Test Conventions

- **Unit tests**: inline `#[cfg(test)] mod tests { ... }` in the same source file.
- **Integration tests**: `tests/*.rs` in the crate directory (e.g. `crates/notedthat-core/tests/`).
- **External-service tests**: annotate with `#[ignore]` and a comment explaining what service is needed:
  ```rust
  #[test]
  #[ignore = "requires SeaweedFS + Qdrant running locally"]
  fn test_full_index_round_trip() { ... }
  ```
- **Run ignored tests**: `cargo test --workspace -- --ignored`
- **Stress scenarios**: prefix the test name with `stress_` on top of `#[ignore]`.
  These generate and hash multi-gibibyte bodies and are meant to be run by hand:

  ```rust
  #[test]
  #[ignore = "generated 5 GiB staging and bounded replay stress scenario"]
  fn stress_stages_and_replays_five_gib_without_retaining_outputs() { ... }
  ```

  CI's integration job runs `--include-ignored --skip stress_`, so the prefix is
  what keeps them out of the release path. Without it a stress scenario runs on
  a GitHub runner, overruns the job timeout, and the cancelled job silently skips
  release-plz — no release, no failure report. Run them locally with
  `cargo test --workspace -- --ignored stress_`.

### Backends in tests

Almost everything runs on in-process substitutes. `notedthat_indexer::testing`
carries `InMemoryVectorStore` and `StubEmbedder`, `notedthat_api_http::testing`
carries `InMemoryStorage`; each is behind that crate's `test-support` feature.
Reach for those first — they need no Docker, and the suites that use them run in
the normal `cargo test` pass rather than behind `#[ignore]`.

To bring up a whole server over them, build a `notedthat_server::run::Backends`
and hand it to `run_with` (enable `notedthat-server`'s `test-support` feature).
Startup, provisioning, the indexer worker, every listener and the shutdown
sequence are the real code path; only S3, Qdrant and the embedding endpoint are
substituted. See `crates/notedthat-server/tests/support/patch_backends.rs`.

Two things the substitutes do not cover, so do not read a green run as covering
them either:

- `QdrantClient`'s `VectorStore` impl — the filter, selector and collection
  translation in `crates/notedthat-indexer/src/qdrant.rs`. That is pinned by the
  conformance suite below, not by the E2E suites.
- `S3Storage` against a real S3 implementation, which is what
  `notedthat-storage-s3`'s container suite is for.

### Container-backed tests

Two suites still need real infrastructure, both `#[ignore]` so they run only
under `--include-ignored`:

- `crates/notedthat-storage-s3/tests/integration.rs` — `S3Storage` against
  SeaweedFS.
- `crates/notedthat-indexer/tests/vector_store_conformance.rs` — runs one
  scenario set through both `QdrantClient` and `InMemoryVectorStore` and asserts
  they agree, so the in-memory substitute cannot drift away from the backend it
  stands in for. Run it after any change to `qdrant.rs` or `testing.rs`.

Both start their container with [testcontainers](https://docs.rs/testcontainers).
**Wait on a readiness signal, never on a fixed duration.** `WaitFor::seconds(5)`
was used throughout and was a reliable source of flakes: on a loaded machine the
container is not ready when the sleep expires, the first RPC lands early, and it
fails against the client timeout with a misleading error — for Qdrant, collection
calls would succeed while the first `upsert_points` returned
`Cancelled: Timeout expired`.

Use the log line each service prints once its listener is bound:

```rust
// Qdrant — stdout
.with_wait_for(WaitFor::message_on_stdout("Qdrant gRPC listening on 6334"))
// SeaweedFS — stderr (it logs to stderr, and takes ~3.5s to reach this line)
.with_wait_for(WaitFor::message_on_stderr("Start Seaweed S3 API Server"))
```

A bound port is not a serving port. The conformance suite polls `health_check`
after the log line before handing the client to a test, and builds its client
through `QdrantClient::new` rather than `Qdrant::from_url(..).build()` — the
latter inherits qdrant-client's 5-second per-RPC default, which a `wait(true)`
upsert can exceed under load.

## Dependency Ownership Rules

- S3/Qdrant/WebDAV deps live **only** in their respective crates (`notedthat-storage-s3`, `notedthat-indexer`, `notedthat-webdav`).
- Shared deps go in `[workspace.dependencies]` in the root `Cargo.toml`, consumed via `foo = { workspace = true }` in member `Cargo.toml` files.
- No inter-crate `path` dependencies in M1 — each crate is standalone until M2 wires them together.

## Adding a New Crate

1. Create the directory: `crates/<name>/`. It must live under `crates/` —
   `.github/workflows/publish-crate-initial.yml` looks nowhere else.
2. Add `Cargo.toml` inheriting workspace fields (`version.workspace = true`, etc.) and `[lints] workspace = true`.
3. Add the path to `members` in the root `Cargo.toml`.
4. Add the crate name to `changelog_include` in `release-plz.toml` (under the `notedthat-server` facade package).
5. Add the crate name to the `options` list in `.github/workflows/publish-crate-manual.yml`.
6. If the crate sets `readme = "README.md"`, write that file, and add a row to the
   crate table in the root [README](README.md).
7. Binaries belong in the `notedthat` distribution crate, not in a new one — it is the
   only package with `[package.metadata.dist] dist = true`, and one cargo-dist app per
   workspace keeps the release to one archive and one installer per target.
8. Bootstrap it on crates.io **before** merging: Trusted Publishing cannot create a new
   crate, and `cargo publish` verifies against the registry, so a crate cannot be
   bootstrapped until its dependencies are published at the same version. See
   [RELEASING.md](RELEASING.md#3-bootstrap-publish-first-time-only).
