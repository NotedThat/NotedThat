//! Tracing / logging initialization for `notedthat-server`.

use crate::config::LogFormat;
use std::ffi::OsStr;
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
/// Returns an error if `RUST_LOG` is set but is not UTF-8 or cannot be parsed
/// as a filter, or if a global tracing subscriber has already been installed.
pub fn init_tracing(format: LogFormat) -> anyhow::Result<()> {
    let env_filter = env_filter(std::env::var_os("RUST_LOG").as_deref())?;

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
fn env_filter(rust_log: Option<&OsStr>) -> anyhow::Result<EnvFilter> {
    let Some(spec) = rust_log else {
        return Ok(EnvFilter::new(DEFAULT_FILTER));
    };
    let spec = spec
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("RUST_LOG must be valid UTF-8"))?;
    // tracing-subscriber 0.3.23 slices each directive untrimmed, so `a, b` or a
    // trailing newline would fail to parse; trim them the way an operator means.
    let trimmed = spec.split(',').map(str::trim).collect::<Vec<_>>().join(",");
    EnvFilter::try_new(trimmed).map_err(|e| anyhow::anyhow!("RUST_LOG is invalid ({spec:?}): {e}"))
}

#[cfg(test)]
mod tests {
    use super::env_filter;
    use std::ffi::OsStr;

    #[test]
    fn unset_rust_log_uses_the_default_filter() {
        let filter = env_filter(None).expect("default filter");
        assert_eq!(filter.to_string(), "notedthat=debug,info");
    }

    #[test]
    fn valid_rust_log_is_used() {
        let filter =
            env_filter(Some(OsStr::new("notedthat=trace,hyper=warn"))).expect("valid filter");
        assert_eq!(filter.to_string(), "notedthat=trace,hyper=warn");
    }

    #[test]
    fn misspelled_level_in_rust_log_refuses_startup() {
        let err =
            env_filter(Some(OsStr::new("notedthat=debgu"))).expect_err("a typo must be refused");
        let msg = err.to_string();
        assert!(msg.contains("RUST_LOG"), "error must name RUST_LOG: {msg}");
    }

    #[test]
    fn space_after_a_comma_in_rust_log_is_accepted() {
        let filter = env_filter(Some(OsStr::new("info, notedthat=debug"))).expect("spaced filter");
        assert_eq!(filter.to_string(), "notedthat=debug,info");
    }

    #[test]
    fn trailing_newline_in_rust_log_is_accepted() {
        let filter = env_filter(Some(OsStr::new("notedthat=debug\n"))).expect("newline filter");
        assert_eq!(filter.to_string(), "notedthat=debug");
    }

    #[test]
    fn whitespace_only_rust_log_is_treated_as_empty() {
        let filter = env_filter(Some(OsStr::new(" \n"))).expect("whitespace-only filter");
        let empty = env_filter(Some(OsStr::new(""))).expect("empty filter");
        assert_eq!(filter.to_string(), empty.to_string());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_rust_log_refuses_startup() {
        use std::os::unix::ffi::OsStrExt;
        let err = env_filter(Some(OsStr::from_bytes(b"notedthat=\xff")))
            .expect_err("a non-UTF-8 value must be refused");
        let msg = err.to_string();
        assert!(msg.contains("RUST_LOG"), "error must name RUST_LOG: {msg}");
    }
}
