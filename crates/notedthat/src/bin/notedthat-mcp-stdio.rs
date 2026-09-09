//! `notedthat-mcp-stdio`: MCP-over-stdio transport for `NotedThat`.
//!
//! Reads `NOTEDTHAT_URL` and `NOTEDTHAT_TOKEN` from the environment,
//! validates them, and serves MCP tools over stdio JSON-RPC.
//!
//! **Stdout is reserved for JSON-RPC.** All log output goes to stderr.
//!
//! The transport itself lives in `notedthat_mcp_stdio`; this target exists so
//! the binary ships from the `notedthat` facade crate.

/// `NotedThat` MCP-over-stdio entry point.
///
/// Delegates to [`notedthat_mcp_stdio::run`], which owns logging setup,
/// environment validation, and the stdio service loop. Returns non-zero on any
/// startup failure (missing or empty `NOTEDTHAT_URL` / `NOTEDTHAT_TOKEN`).
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    notedthat_mcp_stdio::run().await
}
