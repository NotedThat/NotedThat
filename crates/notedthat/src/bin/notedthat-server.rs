//! `notedthat-server` binary entry point.
//!
//! Boots the HTTP API + `WebDAV` + remote MCP server as a single process.

use clap::Parser as _;
use notedthat_server::{cli::ServerCli, config::Config, run::run, tracing_init::init_tracing};

/// `NotedThat` server entry point.
///
/// Resolves configuration from the command line and the environment — a flag
/// wins over the variable it mirrors — initializes tracing, and delegates to
/// [`run`] for all application logic. Returns non-zero on any startup failure
/// (configuration missing or invalid, S3 provisioning error, etc.).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_cli(ServerCli::parse()).map_err(|e| {
        eprintln!("startup: {e}");
        e
    })?;
    init_tracing(config.log_format)?;
    run(config).await
}
