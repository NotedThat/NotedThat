This is a Rust workspace (`crates/`); the Rust toolchain is installed and
the locked dependencies' sources are fetched.

- Search in Rust: `rg -n -t rust 'fn name' crates/`, `fd name crates/`. For
  Rust syntax rather than text, ast-grep:
  `ast-grep run -l rust -p 'axum::body::to_bytes($$$ARGS)' crates/` or
  `ast-grep run -l rust -p 'Verb::$V' crates/notedthat-webdav/`.
- The workspace: `cargo metadata --format-version 1 --no-deps --offline | jq`
  (its crates and targets), `cargo tree --offline -i <crate>` (who depends
  on a crate).
- A dependency's source, to check what a library call really does:
  `~/.cargo/registry/src/*/<crate>-<version>/` (versions are in
  `Cargo.lock`).
- Tests live next to the code (`#[cfg(test)]`) and in `crates/*/tests/`;
  they show the intended contract.
- Do not run `cargo build`, `check`, `test` or `clippy`: CI runs those, and a
  build would use up your turns.
