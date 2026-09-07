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
  endpoints, then runs `cargo run -p notedthat-server`.

Do not commit provider credentials. Copy `.env.example` to `.env`, replace its
embedding endpoint, model, key, and dimensions, then follow the chosen README flow.

## Running a Specific Crate

```sh
cargo run -p notedthat-mcp-stdio
```

## Installing the MCP stdio wrapper

`notedthat-mcp-stdio` is the subprocess an MCP client (opencode, Claude Desktop, Cursor, Zed) spawns to talk to the server. To keep an MCP client wired up against a local build, install the binary somewhere on `PATH`. The Makefile has two paths, both writing to `$HOME/.local/bin/notedthat-mcp-stdio`:

```sh
make mcp-stdio             # cargo install from source — reflects your working tree
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

### Container-backed tests

Tests that need Qdrant or SeaweedFS start them with
[testcontainers](https://docs.rs/testcontainers). **Wait on a readiness signal,
never on a fixed duration.** `WaitFor::seconds(5)` was used throughout and was a
reliable source of flakes: on a loaded machine the container is not ready when the
sleep expires, the first RPC lands early, and it fails against the client timeout
with a misleading error — for Qdrant, collection calls would succeed while the
first `upsert_points` returned `Cancelled: Timeout expired`.

Use the log line each service prints once its listener is bound:

```rust
// Qdrant — stdout
.with_wait_for(WaitFor::message_on_stdout("Qdrant gRPC listening on 6334"))
// SeaweedFS — stderr (it logs to stderr, and takes ~3.5s to reach this line)
.with_wait_for(WaitFor::message_on_stderr("Start Seaweed S3 API Server"))
```

`notedthat-indexer` goes one step further in `tests/support/mod.rs`: after the log
line it polls `health_check` until Qdrant answers, so a bound-but-not-yet-serving
port cannot slip through. Prefer `support::start_qdrant()` and
`support::raw_client()` there over hand-rolling a container or a client in a test
file — a raw `Qdrant::from_url(..).build()` inherits the crate's 5-second
per-RPC default, which a `wait(true)` upsert can exceed under load.

Give the server itself room to start. `build_infrastructure` provisions buckets,
manifests and a Qdrant collection; measured on a cold, loaded machine that is
about 9s, so readiness budgets are 60s rather than the 10s that used to leave
under a second of margin.

## Dependency Ownership Rules

- S3/Qdrant/WebDAV deps live **only** in their respective crates (`notedthat-storage-s3`, `notedthat-indexer`, `notedthat-webdav`).
- Shared deps go in `[workspace.dependencies]` in the root `Cargo.toml`, consumed via `foo = { workspace = true }` in member `Cargo.toml` files.
- No inter-crate `path` dependencies in M1 — each crate is standalone until M2 wires them together.

## Adding a New Crate

1. Create the directory: `crates/<name>/` (or `bin/<name>/` for installable binaries).
2. Add `Cargo.toml` inheriting workspace fields (`version.workspace = true`, etc.) and `[lints] workspace = true`.
3. Add the path to `members` in the root `Cargo.toml`.
4. Add the crate name to `changelog_include` in `release-plz.toml` (under the `notedthat-server` facade package).
5. Add the crate name to the `options` list in `.github/workflows/publish-crate-manual.yml`.
