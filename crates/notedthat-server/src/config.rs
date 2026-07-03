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
    /// Qdrant client configuration.
    pub qdrant: ServerQdrantConfig,
    /// Embedder configuration.
    pub embedder: EmbedderConfig,
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
            return Err(Error::Config {
                message: "NOTEDTHAT_API_TOKEN must not be empty".into(),
            });
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

        let qdrant = ServerQdrantConfig::from_env()?;
        let embedder = EmbedderConfig::from_env()?;

        Ok(Self {
            api_token,
            kbs,
            tenant_slug,
            listen_addr,
            s3,
            log_format,
            qdrant,
            embedder,
        })
    }
}

/// Qdrant client configuration, parsed from env vars.
#[derive(Debug, Clone)]
pub struct ServerQdrantConfig {
    /// Qdrant gRPC/HTTP endpoint (`NOTEDTHAT_QDRANT_URL`; required).
    pub url: String,
    /// Optional Qdrant API key (`NOTEDTHAT_QDRANT_API_KEY`).
    pub api_key: Option<String>,
}

impl ServerQdrantConfig {
    /// Parse Qdrant configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` if `NOTEDTHAT_QDRANT_URL` is missing.
    pub fn from_env() -> Result<Self, Error> {
        let url = std::env::var("NOTEDTHAT_QDRANT_URL").map_err(|_| Error::Config {
            message: "NOTEDTHAT_QDRANT_URL is required".into(),
        })?;
        let api_key = std::env::var("NOTEDTHAT_QDRANT_API_KEY").ok();
        Ok(Self { url, api_key })
    }
}

/// Embedder configuration, parsed from env vars.
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    /// OpenAI-compatible embedding endpoint URL (`EMBEDDING_ENDPOINT_URL`; required).
    pub endpoint_url: String,
    /// Embedding model name (`EMBEDDING_MODEL`; required).
    pub model: String,
    /// API key for the embedding endpoint (`EMBEDDING_API_KEY`; required).
    pub api_key: String,
    /// Output vector dimensions (`EMBEDDING_DIMENSIONS`; required).
    pub dimensions: u32,
    /// Number of texts per embedding batch (`EMBEDDING_BATCH_SIZE`; default `32`).
    pub batch_size: usize,
    /// HTTP request timeout in milliseconds (`EMBEDDING_TIMEOUT_MS`; default `30000`).
    pub timeout_ms: u64,
    /// Maximum number of retries on transient failures (`EMBEDDING_MAX_RETRIES`; default `3`).
    pub max_retries: u32,
    /// Maximum tokens per input text (`EMBEDDING_MAX_INPUT_TOKENS`; default `8192`).
    pub max_input_tokens: usize,
}

impl EmbedderConfig {
    /// Parse embedder configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` if any required variable is missing or invalid.
    pub fn from_env() -> Result<Self, Error> {
        let endpoint_url = std::env::var("EMBEDDING_ENDPOINT_URL").map_err(|_| Error::Config {
            message: "EMBEDDING_ENDPOINT_URL is required".into(),
        })?;
        let model = std::env::var("EMBEDDING_MODEL").map_err(|_| Error::Config {
            message: "EMBEDDING_MODEL is required".into(),
        })?;
        let api_key = std::env::var("EMBEDDING_API_KEY").map_err(|_| Error::Config {
            message: "EMBEDDING_API_KEY is required".into(),
        })?;
        let dimensions: u32 = std::env::var("EMBEDDING_DIMENSIONS")
            .map_err(|_| Error::Config {
                message: "EMBEDDING_DIMENSIONS is required".into(),
            })?
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::Config {
                message: format!("EMBEDDING_DIMENSIONS is invalid: {e}"),
            })?;
        let batch_size: usize = std::env::var("EMBEDDING_BATCH_SIZE")
            .unwrap_or_else(|_| "32".to_string())
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::Config {
                message: format!("EMBEDDING_BATCH_SIZE is invalid: {e}"),
            })?;
        let timeout_ms: u64 = std::env::var("EMBEDDING_TIMEOUT_MS")
            .unwrap_or_else(|_| "30000".to_string())
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::Config {
                message: format!("EMBEDDING_TIMEOUT_MS is invalid: {e}"),
            })?;
        let max_retries: u32 = std::env::var("EMBEDDING_MAX_RETRIES")
            .unwrap_or_else(|_| "3".to_string())
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::Config {
                message: format!("EMBEDDING_MAX_RETRIES is invalid: {e}"),
            })?;
        let max_input_tokens: usize = std::env::var("EMBEDDING_MAX_INPUT_TOKENS")
            .unwrap_or_else(|_| "8192".to_string())
            .parse()
            .map_err(|e: std::num::ParseIntError| Error::Config {
                message: format!("EMBEDDING_MAX_INPUT_TOKENS is invalid: {e}"),
            })?;
        Ok(Self {
            endpoint_url,
            model,
            api_key,
            dimensions,
            batch_size,
            timeout_ms,
            max_retries,
            max_input_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ENV_KEYS: [&str; 19] = [
        "NOTEDTHAT_API_TOKEN",
        "NOTEDTHAT_KBS",
        "NOTEDTHAT_S3_REGION",
        "NOTEDTHAT_S3_ACCESS_KEY_ID",
        "NOTEDTHAT_S3_SECRET_ACCESS_KEY",
        "NOTEDTHAT_LISTEN_ADDR",
        "NOTEDTHAT_LOG_FORMAT",
        "NOTEDTHAT_S3_ENDPOINT_URL",
        "NOTEDTHAT_S3_FORCE_PATH_STYLE",
        "NOTEDTHAT_QDRANT_URL",
        "NOTEDTHAT_QDRANT_API_KEY",
        "EMBEDDING_ENDPOINT_URL",
        "EMBEDDING_MODEL",
        "EMBEDDING_API_KEY",
        "EMBEDDING_DIMENSIONS",
        "EMBEDDING_BATCH_SIZE",
        "EMBEDDING_TIMEOUT_MS",
        "EMBEDDING_MAX_RETRIES",
        "EMBEDDING_MAX_INPUT_TOKENS",
    ];

    fn run_with_env<F: FnOnce() -> R, R>(overrides: &[(&str, Option<&str>)], f: F) -> R {
        let mut vars: Vec<(&str, Option<&str>)> = vec![
            ("NOTEDTHAT_API_TOKEN", Some("test-token")),
            ("NOTEDTHAT_KBS", Some("notes,docs")),
            ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
            ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("key")),
            ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("secret")),
            ("NOTEDTHAT_LISTEN_ADDR", None),
            ("NOTEDTHAT_LOG_FORMAT", None),
            ("NOTEDTHAT_S3_ENDPOINT_URL", None),
            ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
            ("NOTEDTHAT_QDRANT_URL", Some("http://localhost:6334")),
            ("NOTEDTHAT_QDRANT_API_KEY", None),
            ("EMBEDDING_ENDPOINT_URL", Some("https://api.openai.com")),
            ("EMBEDDING_MODEL", Some("text-embedding-3-small")),
            ("EMBEDDING_API_KEY", Some("sk-test")),
            ("EMBEDDING_DIMENSIONS", Some("1536")),
            ("EMBEDDING_BATCH_SIZE", None),
            ("EMBEDDING_TIMEOUT_MS", None),
            ("EMBEDDING_MAX_RETRIES", None),
            ("EMBEDDING_MAX_INPUT_TOKENS", None),
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("at least one knowledge base")
        );
    }

    #[test]
    fn test_duplicate_slug_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_KBS", Some("notes,notes"))], Config::from_env);
        assert!(result.is_err(), "duplicate slugs should fail");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("duplicate"),
            "error should mention 'duplicate'"
        );
    }

    #[test]
    fn test_no_tenant_slug_env_var() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.tenant_slug.as_str(), "default");
    }

    #[test]
    fn test_log_format_json() {
        let cfg =
            run_with_env(&[("NOTEDTHAT_LOG_FORMAT", Some("json"))], Config::from_env).unwrap();
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("NOTEDTHAT_API_TOKEN")
        );
    }

    #[test]
    fn all_env_keys_are_accounted_for() {
        assert_eq!(ALL_ENV_KEYS.len(), 19);
    }

    #[test]
    fn qdrant_url_missing_returns_error() {
        let result = run_with_env(&[("NOTEDTHAT_QDRANT_URL", None)], Config::from_env);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("NOTEDTHAT_QDRANT_URL"),
            "error should mention the missing var: {msg}"
        );
    }

    #[test]
    fn qdrant_api_key_optional() {
        let cfg = run_with_env(&[("NOTEDTHAT_QDRANT_API_KEY", None)], Config::from_env).unwrap();
        assert!(
            cfg.qdrant.api_key.is_none(),
            "api_key should be None when env var is unset"
        );
    }

    #[test]
    fn qdrant_api_key_set_when_present() {
        let cfg = run_with_env(
            &[("NOTEDTHAT_QDRANT_API_KEY", Some("my-secret-key"))],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.qdrant.api_key.as_deref(), Some("my-secret-key"));
    }

    #[test]
    fn qdrant_url_propagated_to_config() {
        let cfg = run_with_env(
            &[(
                "NOTEDTHAT_QDRANT_URL",
                Some("http://qdrant.example.com:6334"),
            )],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.qdrant.url, "http://qdrant.example.com:6334");
    }

    #[test]
    fn embedding_endpoint_url_missing() {
        let result = run_with_env(&[("EMBEDDING_ENDPOINT_URL", None)], Config::from_env);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EMBEDDING_ENDPOINT_URL"),
            "error should mention the missing var: {msg}"
        );
    }

    #[test]
    fn embedding_model_missing() {
        let result = run_with_env(&[("EMBEDDING_MODEL", None)], Config::from_env);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EMBEDDING_MODEL"),
            "error should mention the missing var: {msg}"
        );
    }

    #[test]
    fn embedding_api_key_missing() {
        let result = run_with_env(&[("EMBEDDING_API_KEY", None)], Config::from_env);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EMBEDDING_API_KEY"),
            "error should mention the missing var: {msg}"
        );
    }

    #[test]
    fn embedding_dimensions_missing() {
        let result = run_with_env(&[("EMBEDDING_DIMENSIONS", None)], Config::from_env);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EMBEDDING_DIMENSIONS"),
            "error should mention the missing var: {msg}"
        );
    }

    #[test]
    fn embedding_dimensions_invalid() {
        let result = run_with_env(
            &[("EMBEDDING_DIMENSIONS", Some("not-a-number"))],
            Config::from_env,
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("EMBEDDING_DIMENSIONS"),
            "error should mention the invalid var: {msg}"
        );
    }

    #[test]
    fn embedding_batch_size_default() {
        let cfg = run_with_env(&[("EMBEDDING_BATCH_SIZE", None)], Config::from_env).unwrap();
        assert_eq!(cfg.embedder.batch_size, 32);
    }

    #[test]
    fn embedding_timeout_ms_default() {
        let cfg = run_with_env(&[("EMBEDDING_TIMEOUT_MS", None)], Config::from_env).unwrap();
        assert_eq!(cfg.embedder.timeout_ms, 30_000);
    }

    #[test]
    fn embedding_max_retries_default() {
        let cfg = run_with_env(&[("EMBEDDING_MAX_RETRIES", None)], Config::from_env).unwrap();
        assert_eq!(cfg.embedder.max_retries, 3);
    }

    #[test]
    fn embedding_max_input_tokens_default() {
        let cfg = run_with_env(&[("EMBEDDING_MAX_INPUT_TOKENS", None)], Config::from_env).unwrap();
        assert_eq!(cfg.embedder.max_input_tokens, 8192);
    }

    #[test]
    fn embedder_fields_propagated_to_config() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.embedder.endpoint_url, "https://api.openai.com");
        assert_eq!(cfg.embedder.model, "text-embedding-3-small");
        assert_eq!(cfg.embedder.api_key, "sk-test");
        assert_eq!(cfg.embedder.dimensions, 1536);
    }
}
