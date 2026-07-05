#![allow(missing_docs)]

/// Verify that the notedthat-mcp-stdio binary is available for integration tests.
/// This test ensures the binary can be resolved for cross-transport test scenarios
/// (e.g., spawning stdio transport in tests).
#[test]
fn mcp_stdio_binary_is_available() {
    // The binary is available as a dev-dependency and can be invoked via cargo run
    // For integration tests that need to spawn the stdio transport, use:
    //   std::process::Command::new("cargo")
    //     .args(&["run", "-p", "notedthat-mcp-stdio", "--"])
    //     .spawn()
    //
    // Or build it explicitly and reference the target path:
    //   target/debug/notedthat-mcp-stdio

    // Verify the binary target exists in the workspace
    // The test runs from the workspace root, so use relative path
    let stdio_crate = std::path::Path::new("../notedthat-mcp-stdio");
    assert!(
        stdio_crate.exists(),
        "notedthat-mcp-stdio crate must exist at {}",
        stdio_crate.display()
    );

    // Verify Cargo.toml has the binary target
    let cargo_toml = stdio_crate.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml)
        .expect("Failed to read notedthat-mcp-stdio Cargo.toml");
    assert!(
        content.contains("[[bin]]") && content.contains("name = \"notedthat-mcp-stdio\""),
        "notedthat-mcp-stdio must have a [[bin]] target in Cargo.toml"
    );
}
