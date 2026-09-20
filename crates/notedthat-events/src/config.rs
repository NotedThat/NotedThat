//! Configuration for each event log adapter, parsed from `NOTEDTHAT_*` settings.
//!
//! The selector itself (`NOTEDTHAT_EVENTS_BACKEND`) is `notedthat-server`'s: it
//! decides which of these to build and rejects the other adapter's variables
//! the way it does for storage (§6.5). What lives here is one settings struct
//! and one validator per adapter, in the shape of `notedthat-storage-s3`.

use std::ffi::{OsStr, OsString};
use std::time::Duration;

use notedthat_core::{Error, setting};

/// Every environment variable the `memory` adapter reads.
pub const MEMORY_ENV_VARS: [&str; 1] = ["NOTEDTHAT_EVENTS_MEMORY_CAPACITY"];

/// Every environment variable the `nats` adapter reads.
pub const NATS_ENV_VARS: [&str; 3] = [
    "NOTEDTHAT_NATS_URL",
    "NOTEDTHAT_NATS_STREAM",
    "NOTEDTHAT_NATS_MAX_AGE_SECS",
];

/// Events the ring keeps before the oldest is dropped, unless configured.
pub const DEFAULT_MEMORY_CAPACITY: usize = 10_000;
/// The `JetStream` stream name unless configured.
pub const DEFAULT_NATS_STREAM: &str = "notedthat-events";
/// Retention unless configured: seven days.
pub const DEFAULT_NATS_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// The raw, unvalidated value of every `memory` setting.
///
/// `None` means not supplied; an empty string is a supplied value.
#[derive(Debug, Clone, Default)]
pub struct MemorySettings {
    /// `NOTEDTHAT_EVENTS_MEMORY_CAPACITY`.
    pub capacity: Option<OsString>,
}

impl MemorySettings {
    /// Collect every setting from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            capacity: std::env::var_os("NOTEDTHAT_EVENTS_MEMORY_CAPACITY"),
        }
    }
}

/// The `memory` adapter's validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConfig {
    /// How many events the ring retains for replay.
    pub capacity: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_MEMORY_CAPACITY,
        }
    }
}

impl MemoryConfig {
    /// Validate already-collected settings, whatever supplied them.
    ///
    /// # Errors
    ///
    /// `Error::Config` when the capacity is not a positive integer.
    pub fn from_settings(settings: &MemorySettings) -> Result<Self, Error> {
        let capacity = parse_positive(
            "NOTEDTHAT_EVENTS_MEMORY_CAPACITY",
            settings.capacity.as_deref(),
            DEFAULT_MEMORY_CAPACITY as u64,
            "a positive number of events",
        )?;
        Ok(Self {
            capacity: usize::try_from(capacity).map_err(|_| {
                config_error(format!(
                    "{} is invalid: {capacity} does not fit this platform",
                    setting("NOTEDTHAT_EVENTS_MEMORY_CAPACITY")
                ))
            })?,
        })
    }
}

/// The raw, unvalidated value of every `nats` setting.
#[derive(Debug, Clone, Default)]
pub struct NatsSettings {
    /// `NOTEDTHAT_NATS_URL`.
    pub url: Option<String>,
    /// `NOTEDTHAT_NATS_STREAM`.
    pub stream: Option<String>,
    /// `NOTEDTHAT_NATS_MAX_AGE_SECS`.
    pub max_age_secs: Option<OsString>,
}

impl NatsSettings {
    /// Collect every setting from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            url: std::env::var("NOTEDTHAT_NATS_URL").ok(),
            stream: std::env::var("NOTEDTHAT_NATS_STREAM").ok(),
            max_age_secs: std::env::var_os("NOTEDTHAT_NATS_MAX_AGE_SECS"),
        }
    }
}

/// The `nats` adapter's validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatsConfig {
    /// Server URL, `nats://[user:pass@]host:port`. Credentials, if any, travel
    /// in the URL, which is why the server hides this value from `--help`.
    pub url: String,
    /// The `JetStream` stream that holds the log. Created if absent.
    pub stream: String,
    /// How long the stream keeps an event before retaining it out.
    pub max_age: Duration,
}

impl NatsConfig {
    /// Validate already-collected settings, whatever supplied them.
    ///
    /// # Errors
    ///
    /// `Error::Config` when the URL is absent or empty, the stream name has
    /// characters `JetStream` refuses, or the retention is not a positive number
    /// of seconds.
    pub fn from_settings(settings: NatsSettings) -> Result<Self, Error> {
        let url = settings.url.filter(|url| !url.is_empty()).ok_or_else(|| {
            config_error(format!("{} is required", setting("NOTEDTHAT_NATS_URL")))
        })?;

        let stream = match settings.stream {
            None => DEFAULT_NATS_STREAM.to_string(),
            Some(name) if name.is_empty() => {
                return Err(config_error(format!(
                    "{} must not be empty",
                    setting("NOTEDTHAT_NATS_STREAM")
                )));
            }
            Some(name) => {
                if !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(config_error(format!(
                        "{} is invalid: expected letters, digits, '_' or '-', got \"{name}\"",
                        setting("NOTEDTHAT_NATS_STREAM")
                    )));
                }
                name
            }
        };

        let max_age_secs = parse_positive(
            "NOTEDTHAT_NATS_MAX_AGE_SECS",
            settings.max_age_secs.as_deref(),
            DEFAULT_NATS_MAX_AGE_SECS,
            "a positive number of seconds",
        )?;

        Ok(Self {
            url,
            stream,
            max_age: Duration::from_secs(max_age_secs),
        })
    }
}

fn parse_positive(
    var: &str,
    supplied: Option<&OsStr>,
    default: u64,
    accepted: &str,
) -> Result<u64, Error> {
    let Some(value) = supplied else {
        return Ok(default);
    };
    let value = value
        .to_str()
        .ok_or_else(|| config_error(format!("{} must be valid UTF-8", setting(var))))?;
    if value.is_empty() {
        return Err(config_error(format!("{} must not be empty", setting(var))));
    }
    value.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
        config_error(format!(
            "{} is invalid: expected {accepted}, got \"{value}\"",
            setting(var)
        ))
    })
}

fn config_error(message: String) -> Error {
    Error::Config { message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(err: Error) -> String {
        match err {
            Error::Config { message } => message,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn memory_defaults_when_nothing_is_supplied() {
        let config = MemoryConfig::from_settings(&MemorySettings::default()).unwrap();
        assert_eq!(config, MemoryConfig::default());
        assert_eq!(config.capacity, DEFAULT_MEMORY_CAPACITY);
    }

    #[test]
    fn memory_capacity_is_a_positive_integer() {
        let ok = MemoryConfig::from_settings(&MemorySettings {
            capacity: Some("250".into()),
        })
        .unwrap();
        assert_eq!(ok.capacity, 250);

        for bad in ["0", "-1", "lots", ""] {
            let err = message(
                MemoryConfig::from_settings(&MemorySettings {
                    capacity: Some(bad.into()),
                })
                .unwrap_err(),
            );
            assert!(
                err.contains("NOTEDTHAT_EVENTS_MEMORY_CAPACITY (--events-memory-capacity)"),
                "{err}"
            );
        }
    }

    #[test]
    fn nats_requires_a_url_and_names_the_setting() {
        let err = message(NatsConfig::from_settings(NatsSettings::default()).unwrap_err());
        assert_eq!(err, "NOTEDTHAT_NATS_URL (--nats-url) is required");

        let err = message(
            NatsConfig::from_settings(NatsSettings {
                url: Some(String::new()),
                ..NatsSettings::default()
            })
            .unwrap_err(),
        );
        assert_eq!(err, "NOTEDTHAT_NATS_URL (--nats-url) is required");
    }

    #[test]
    fn nats_defaults_fill_the_stream_and_retention() {
        let config = NatsConfig::from_settings(NatsSettings {
            url: Some("nats://localhost:4222".into()),
            ..NatsSettings::default()
        })
        .unwrap();
        assert_eq!(config.url, "nats://localhost:4222");
        assert_eq!(config.stream, DEFAULT_NATS_STREAM);
        assert_eq!(
            config.max_age,
            Duration::from_secs(DEFAULT_NATS_MAX_AGE_SECS)
        );
    }

    #[test]
    fn nats_stream_names_are_checked_the_way_jetstream_checks_them() {
        let ok = NatsConfig::from_settings(NatsSettings {
            url: Some("nats://localhost:4222".into()),
            stream: Some("nt_events-2".into()),
            ..NatsSettings::default()
        })
        .unwrap();
        assert_eq!(ok.stream, "nt_events-2");

        for bad in ["with.dot", "with space", "wild*", ""] {
            let err = message(
                NatsConfig::from_settings(NatsSettings {
                    url: Some("nats://localhost:4222".into()),
                    stream: Some(bad.into()),
                    ..NatsSettings::default()
                })
                .unwrap_err(),
            );
            assert!(
                err.contains("NOTEDTHAT_NATS_STREAM (--nats-stream)"),
                "{err}"
            );
        }
    }

    #[test]
    fn nats_retention_is_positive_seconds() {
        let ok = NatsConfig::from_settings(NatsSettings {
            url: Some("nats://localhost:4222".into()),
            max_age_secs: Some("3600".into()),
            ..NatsSettings::default()
        })
        .unwrap();
        assert_eq!(ok.max_age, Duration::from_secs(3600));

        for bad in ["0", "7d", ""] {
            let err = message(
                NatsConfig::from_settings(NatsSettings {
                    url: Some("nats://localhost:4222".into()),
                    max_age_secs: Some(bad.into()),
                    ..NatsSettings::default()
                })
                .unwrap_err(),
            );
            assert!(
                err.contains("NOTEDTHAT_NATS_MAX_AGE_SECS (--nats-max-age-secs)"),
                "{err}"
            );
        }
    }

    #[test]
    fn from_env_reads_exactly_the_inventoried_variables() {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_EVENTS_MEMORY_CAPACITY", Some("42")),
                ("NOTEDTHAT_NATS_URL", Some("nats://u:p@broker:4222")),
                ("NOTEDTHAT_NATS_STREAM", Some("evt")),
                ("NOTEDTHAT_NATS_MAX_AGE_SECS", Some("60")),
            ],
            || {
                let memory = MemoryConfig::from_settings(&MemorySettings::from_env()).unwrap();
                assert_eq!(memory.capacity, 42);
                let nats = NatsConfig::from_settings(NatsSettings::from_env()).unwrap();
                assert_eq!(nats.url, "nats://u:p@broker:4222");
                assert_eq!(nats.stream, "evt");
                assert_eq!(nats.max_age, Duration::from_secs(60));
            },
        );
    }

    #[test]
    fn the_inventories_carry_the_binary_prefix() {
        for var in MEMORY_ENV_VARS.iter().chain(NATS_ENV_VARS.iter()) {
            assert!(var.starts_with("NOTEDTHAT_"), "{var}");
        }
    }
}
