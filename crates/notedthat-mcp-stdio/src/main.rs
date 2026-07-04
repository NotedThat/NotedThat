//! `notedthat-mcp-stdio`: MCP-over-stdio transport for `NotedThat`.
//!
//! Reads `NOTEDTHAT_URL` and `NOTEDTHAT_TOKEN` from the environment,
//! validates them, and serves MCP tools over stdio JSON-RPC.
//!
//! **Stdout is reserved for JSON-RPC.** All log output goes to stderr.

use anyhow::{bail, Context as _, Result};
use notedthat_mcp::{NotedThatMcp, client::NotedThatClient};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    init_logging();

    let url = require_env("NOTEDTHAT_URL")?;
    let token = require_env("NOTEDTHAT_TOKEN")?;

    let client = NotedThatClient::new(&url, &token)
        .context("invalid NOTEDTHAT_URL or NOTEDTHAT_TOKEN")?;

    tracing::info!(
        target: "notedthat_mcp_stdio",
        "notedthat-mcp-stdio starting; url = {}",
        client.base_url_display()
    );

    let service = NotedThatMcp::new(client)
        .serve(stdio())
        .await
        .context("stdio transport failed")?;

    service.waiting().await.context("service loop failed")?;

    Ok(())
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr) // CRITICAL: stdout reserved for JSON-RPC
        .with_ansi(false)
        .init();
}

/// Read an environment variable, trim whitespace, reject empty-after-trim.
fn require_env(name: &str) -> Result<String> {
    let v = std::env::var(name)
        .map_err(|_| anyhow::anyhow!("{name} is required but not set"))?;
    let trimmed = v.trim();
    if trimmed.is_empty() {
        bail!("{name} is required but is empty");
    }
    Ok(trimmed.to_string())
}
