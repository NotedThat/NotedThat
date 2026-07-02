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

## Running a Specific Crate

```sh
cargo run -p notedthat-mcp-stdio
```

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
