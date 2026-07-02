//! Environment-variable-based configuration for `notedthat-server`.
//!
//! There are no CLI flags and no config files — env vars are the only
//! configuration surface. See `docs/CONFIGURATION.md` for the full reference.

use notedthat_core::{Error, KbSlug, TenantSlug};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Server-wide configuration, parsed from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Static Bearer token for API authentication (`NOTEDTHAT_API_TOKEN`).
    pub api_token: String,
    /// Declared knowledge bases, as a sorted map of slug string → [`KbSlug`].
    pub kbs: BTreeMap<String, KbSlug>,
    /// Tenant slug — hardcoded to `"default"` per Metis directive.
    pub tenant_slug: TenantSlug,
    /// Socket address the HTTP server binds to (`NOTEDTHAT_LISTEN_ADDR`; default `0.0.0.0:8080`).
    pub listen_addr: SocketAddr,
    /// S3 client configuration.
    pub s3: notedthat_storage_s3::S3Config,
    /// Log output format (`NOTEDTHAT_LOG_FORMAT`; `pretty` or `json`).
    pub log_format: LogFormat,
}

/// Tracing output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable multi-line output (default).
    Pretty,
    /// Machine-readable JSON (one line per event).
    Json,
}

impl Config {
    /// Parse configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` if any required variable is missing
    /// or if any value is invalid (empty token, bad slug, duplicate slug, etc.).
    pub fn from_env() -> Result<Self, Error> {
        let api_token = std::env::var("NOTEDTHAT_API_TOKEN").map_err(|_| Error::Config {
            message: "NOTEDTHAT_API_TOKEN is required".into(),
        })?;
        if api_token.is_empty() {
            return Err(Error::Config { message: "NOTEDTHAT_API_TOKEN must not be empty".into() });
        }

        let kbs_raw = std::env::var("NOTEDTHAT_KBS").map_err(|_| Error::Config {
            message: "NOTEDTHAT_KBS is required".into(),
        })?;
        if kbs_raw.trim().is_empty() {
            return Err(Error::Config {
                message: "NOTEDTHAT_KBS must declare at least one knowledge base".into(),
            });
        }

        let mut kbs = BTreeMap::new();
        for token in kbs_raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let slug = KbSlug::try_new(token).map_err(|e| Error::Config {
                message: format!("invalid KB slug {token:?}: {e}"),
            })?;
            if kbs.insert(slug.as_str().to_string(), slug).is_some() {
                return Err(Error::Config {
                    message: format!("duplicate KB slug in NOTEDTHAT_KBS: {token:?}"),
                });
            }
        }
        if kbs.is_empty() {
            return Err(Error::Config {
                message: "NOTEDTHAT_KBS must declare at least one knowledge base".into(),
            });
        }

        // Tenant slug is hardcoded to "default" per Metis directive.
        // NOTEDTHAT_TENANT_SLUG env var intentionally not read.
        let tenant_slug = TenantSlug::default();

        let listen_addr_str =
            std::env::var("NOTEDTHAT_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let listen_addr: SocketAddr = listen_addr_str.parse().map_err(|e| Error::Config {
            message: format!("NOTEDTHAT_LISTEN_ADDR is invalid: {e}"),
        })?;

        let s3 = notedthat_storage_s3::S3Config::from_env()?;

        let log_format = match std::env::var("NOTEDTHAT_LOG_FORMAT").as_deref() {
            Ok("json") => LogFormat::Json,
            _ => LogFormat::Pretty,
        };

        Ok(Self { api_token, kbs, tenant_slug, listen_addr, s3, log_format })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ENV_KEYS: [&str; 9] = [
        "NOTEDTHAT_API_TOKEN",
        "NOTEDTHAT_KBS",
        "NOTEDTHAT_S3_REGION",
        "NOTEDTHAT_S3_ACCESS_KEY_ID",
        "NOTEDTHAT_S3_SECRET_ACCESS_KEY",
        "NOTEDTHAT_LISTEN_ADDR",
        "NOTEDTHAT_LOG_FORMAT",
        "NOTEDTHAT_S3_ENDPOINT_URL",
        "NOTEDTHAT_S3_FORCE_PATH_STYLE",
    ];

    fn run_with_env<F: FnOnce() -> R, R>(overrides: &[(&str, Option<&str>)], f: F) -> R {
        let mut vars = vec![
            ("NOTEDTHAT_API_TOKEN", Some("test-token")),
            ("NOTEDTHAT_KBS", Some("notes,docs")),
            ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
            ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("key")),
            ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("secret")),
            ("NOTEDTHAT_LISTEN_ADDR", None),
            ("NOTEDTHAT_LOG_FORMAT", None),
            ("NOTEDTHAT_S3_ENDPOINT_URL", None),
            ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
        ];

        for (key, value) in overrides {
            if let Some((_, slot)) = vars.iter_mut().find(|(existing, _)| existing == key) {
                *slot = *value;
            }
        }

        temp_env::with_vars(vars, f)
    }

    #[test]
    fn test_empty_kbs_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_KBS", Some(""))], Config::from_env);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least one knowledge base"));
    }

    #[test]
    fn test_duplicate_slug_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_KBS", Some("notes,notes"))], Config::from_env);
        assert!(result.is_err(), "duplicate slugs should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("duplicate"), "error should mention 'duplicate'");
    }

    #[test]
    fn test_no_tenant_slug_env_var() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.tenant_slug.as_str(), "default");
    }

    #[test]
    fn test_log_format_json() {
        let cfg = run_with_env(&[("NOTEDTHAT_LOG_FORMAT", Some("json"))], Config::from_env).unwrap();
        assert_eq!(cfg.log_format, LogFormat::Json);
    }

    #[test]
    fn test_log_format_default_pretty() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.log_format, LogFormat::Pretty);
    }

    #[test]
    fn test_default_listen_addr() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.listen_addr.to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn test_invalid_listen_addr() {
        let result = run_with_env(
            &[("NOTEDTHAT_LISTEN_ADDR", Some("not-a-socket-addr"))],
            Config::from_env,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_kbs_parsed_correctly() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.kbs.len(), 2);
        assert!(cfg.kbs.contains_key("notes"));
        assert!(cfg.kbs.contains_key("docs"));
    }

    #[test]
    fn test_missing_api_token_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_API_TOKEN", None)], Config::from_env);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("NOTEDTHAT_API_TOKEN"));
    }

    #[test]
    fn all_env_keys_are_accounted_for() {
        assert_eq!(ALL_ENV_KEYS.len(), 9);
    }
}
