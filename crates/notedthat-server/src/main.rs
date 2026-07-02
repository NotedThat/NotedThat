//! `notedthat-server` binary entry point.

use notedthat_server::{config::Config, run::run, tracing_init::init_tracing};

/// `NotedThat` server entry point.
///
/// Parses configuration from environment variables, initializes tracing,
/// and delegates to [`run`] for all application logic. Returns non-zero on
/// any startup failure (config missing/invalid, S3 provisioning error, etc.).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env().map_err(|e| {
        eprintln!("startup: {e}");
        e
    })?;
    init_tracing(config.log_format)?;
    run(config).await
}
