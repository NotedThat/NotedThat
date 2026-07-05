//! Build script for `notedthat-server`.
//!
//! Exports `CARGO_BIN_EXE_notedthat-mcp-stdio` so integration tests can locate
//! the `notedthat-mcp-stdio` binary from a cross-package dev-dependency.

use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should be under target/<profile>/build/<pkg>/out");
    let exe_name = if cfg!(windows) {
        "notedthat-mcp-stdio.exe"
    } else {
        "notedthat-mcp-stdio"
    };
    let exe_path = profile_dir.join(exe_name);

    println!(
        "cargo:rustc-env=CARGO_BIN_EXE_notedthat-mcp-stdio={}",
        exe_path.display()
    );
}
