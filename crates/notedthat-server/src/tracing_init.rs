//! Tracing / logging initialization for `notedthat-server`.

use crate::config::LogFormat;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialize the `tracing` subscriber with the requested output format.
///
/// Must be called exactly once, before any `tracing::info!` / `warn!` / etc.
/// calls. Uses [`EnvFilter::try_from_default_env`] so the `RUST_LOG` env var
/// controls verbosity; falls back to `info,notedthat=debug` if unset.
///
/// # Errors
///
/// Returns an error if a global tracing subscriber has already been installed.
pub fn init_tracing(format: LogFormat) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,notedthat=debug"));

    let subscriber = tracing_subscriber::registry().with(env_filter);

    match format {
        LogFormat::Json => subscriber.with(fmt::layer().json()).try_init(),
        LogFormat::Pretty => subscriber.with(fmt::layer().pretty()).try_init(),
    }
    .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))?;

    Ok(())
}
