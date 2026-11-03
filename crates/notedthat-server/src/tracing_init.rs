//! Tracing / logging initialization for `notedthat-server`.

use crate::config::LogFormat;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// The filter used when `RUST_LOG` is unset.
const DEFAULT_FILTER: &str = "info,notedthat=debug";

/// Initialize the `tracing` subscriber with the requested output format.
///
/// Must be called exactly once, before any `tracing::info!` / `warn!` / etc.
/// calls. The `RUST_LOG` env var controls verbosity; falls back to
/// `info,notedthat=debug` if unset.
///
/// # Errors
///
/// Returns an error if `RUST_LOG` is set but cannot be parsed as a filter, or
/// if a global tracing subscriber has already been installed.
pub fn init_tracing(format: LogFormat) -> anyhow::Result<()> {
    let env_filter = env_filter(std::env::var("RUST_LOG").ok().as_deref())?;

    let subscriber = tracing_subscriber::registry().with(env_filter);

    match format {
        LogFormat::Json => subscriber.with(fmt::layer().json()).try_init(),
        LogFormat::Pretty => subscriber.with(fmt::layer().pretty()).try_init(),
    }
    .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))?;

    Ok(())
}

/// Build the filter from the value of `RUST_LOG` (`None` when unset).
///
/// A set-but-unparsable value is refused by name rather than folded into the
/// default: an operator who typos `RUST_LOG` while chasing an incident would
/// otherwise get the default level silently and wait for lines that never come.
fn env_filter(rust_log: Option<&str>) -> anyhow::Result<EnvFilter> {
    match rust_log {
        Some(spec) => EnvFilter::try_new(spec)
            .map_err(|e| anyhow::anyhow!("RUST_LOG is invalid ({spec:?}): {e}")),
        None => Ok(EnvFilter::new(DEFAULT_FILTER)),
    }
}

#[cfg(test)]
mod tests {
    use super::env_filter;

    #[test]
    fn unset_rust_log_uses_the_default_filter() {
        let filter = env_filter(None).expect("default filter");
        assert_eq!(filter.to_string(), "notedthat=debug,info");
    }

    #[test]
    fn valid_rust_log_is_used() {
        let filter = env_filter(Some("notedthat=trace,hyper=warn")).expect("valid filter");
        assert_eq!(filter.to_string(), "notedthat=trace,hyper=warn");
    }

    #[test]
    fn misspelled_level_in_rust_log_refuses_startup() {
        let err = env_filter(Some("notedthat=debgu")).expect_err("a typo must be refused");
        let msg = err.to_string();
        assert!(msg.contains("RUST_LOG"), "error must name RUST_LOG: {msg}");
    }
}
