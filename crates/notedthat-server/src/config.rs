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
    /// Socket address the `WebDAV` server binds to (`NOTEDTHAT_WEBDAV_LISTEN_ADDR`; default `0.0.0.0:8081`).
    pub webdav_listen_addr: SocketAddr,
    /// `WebDAV` Basic authentication username (`NOTEDTHAT_WEBDAV_USERNAME`; required).
    pub webdav_username: String,
    /// `WebDAV` Basic authentication password (`NOTEDTHAT_WEBDAV_PASSWORD`; required).
    pub webdav_password: String,
    /// Socket address the MCP HTTP server binds to (`NOTEDTHAT_MCP_HTTP_BIND`; default `0.0.0.0:8082`).
    pub mcp_http_bind: SocketAddr,
    /// Whether the MCP HTTP listener is enabled (`NOTEDTHAT_MCP_HTTP_ENABLED`; default `true`).
    pub mcp_http_enabled: bool,
    /// Allowed origins for MCP HTTP CORS (`NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS`; empty → `["null"]`).
    pub mcp_http_allowed_origins: Vec<String>,
    /// Allowed hosts for MCP HTTP Host header validation (`NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS`; empty → `["127.0.0.1", "localhost", "::1"]`).
    pub mcp_http_allowed_hosts: Vec<String>,
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

        let webdav_username =
            std::env::var("NOTEDTHAT_WEBDAV_USERNAME").map_err(|_| Error::Config {
                message: "NOTEDTHAT_WEBDAV_USERNAME is required".into(),
            })?;
        if webdav_username.is_empty() {
            return Err(Error::Config {
                message: "NOTEDTHAT_WEBDAV_USERNAME is required and must not be empty".into(),
            });
        }

        let webdav_password =
            std::env::var("NOTEDTHAT_WEBDAV_PASSWORD").map_err(|_| Error::Config {
                message: "NOTEDTHAT_WEBDAV_PASSWORD is required".into(),
            })?;
        if webdav_password.is_empty() {
            return Err(Error::Config {
                message: "NOTEDTHAT_WEBDAV_PASSWORD is required and must not be empty".into(),
            });
        }

        let webdav_listen_addr = std::env::var("NOTEDTHAT_WEBDAV_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8081".to_string())
            .parse::<SocketAddr>()
            .map_err(|e| Error::Config {
                message: format!("NOTEDTHAT_WEBDAV_LISTEN_ADDR is invalid: {e}"),
            })?;

        let mcp_http_enabled = match std::env::var("NOTEDTHAT_MCP_HTTP_ENABLED").as_deref() {
            Ok("false" | "0") => false,
            _ => true,
        };

        let mcp_http_bind = std::env::var("NOTEDTHAT_MCP_HTTP_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8082".to_string())
            .parse::<SocketAddr>()
            .map_err(|e| Error::Config {
                message: format!("NOTEDTHAT_MCP_HTTP_BIND is invalid: {e}"),
            })?;

        // Fail-fast: if MCP HTTP is enabled and API token is empty/whitespace, reject startup.
        if mcp_http_enabled && api_token.trim().is_empty() {
            return Err(Error::Config {
                message: "NOTEDTHAT_API_TOKEN must not be empty when MCP HTTP is enabled".into(),
            });
        }

        let mcp_http_allowed_origins =
            match std::env::var("NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS").as_deref() {
                Ok(s) if !s.trim().is_empty() => s
                    .split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect(),
                _ => vec!["null".to_string()],
            };

        let mcp_http_allowed_hosts =
            match std::env::var("NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS").as_deref() {
                Ok(s) if !s.trim().is_empty() => s
                    .split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect(),
                _ => vec![
                    "127.0.0.1".to_string(),
                    "localhost".to_string(),
                    "::1".to_string(),
                ],
            };

        Ok(Self {
            api_token,
            kbs,
            tenant_slug,
            listen_addr,
            s3,
            log_format,
            qdrant,
            embedder,
            webdav_listen_addr,
            webdav_username,
            webdav_password,
            mcp_http_bind,
            mcp_http_enabled,
            mcp_http_allowed_origins,
            mcp_http_allowed_hosts,
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

    const ALL_ENV_KEYS: [&str; 26] = [
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
        "NOTEDTHAT_WEBDAV_USERNAME",
        "NOTEDTHAT_WEBDAV_PASSWORD",
        "NOTEDTHAT_WEBDAV_LISTEN_ADDR",
        "NOTEDTHAT_MCP_HTTP_BIND",
        "NOTEDTHAT_MCP_HTTP_ENABLED",
        "NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS",
        "NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS",
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
            ("NOTEDTHAT_WEBDAV_USERNAME", Some("webdav-user")),
            ("NOTEDTHAT_WEBDAV_PASSWORD", Some("webdav-pass")),
            ("NOTEDTHAT_WEBDAV_LISTEN_ADDR", None),
            ("NOTEDTHAT_MCP_HTTP_BIND", None),
            ("NOTEDTHAT_MCP_HTTP_ENABLED", None),
            ("NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS", None),
            ("NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS", None),
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
    fn test_missing_webdav_username_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_WEBDAV_USERNAME", None)], Config::from_env);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("NOTEDTHAT_WEBDAV_USERNAME")
        );
    }

    #[test]
    fn test_empty_webdav_username_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_WEBDAV_USERNAME", Some(""))], Config::from_env);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("NOTEDTHAT_WEBDAV_USERNAME")
        );
    }

    #[test]
    fn test_missing_webdav_password_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_WEBDAV_PASSWORD", None)], Config::from_env);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("NOTEDTHAT_WEBDAV_PASSWORD")
        );
    }

    #[test]
    fn test_empty_webdav_password_rejected() {
        let result = run_with_env(&[("NOTEDTHAT_WEBDAV_PASSWORD", Some(""))], Config::from_env);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("NOTEDTHAT_WEBDAV_PASSWORD")
        );
    }

    #[test]
    fn test_default_webdav_listen_addr() {
        let cfg =
            run_with_env(&[("NOTEDTHAT_WEBDAV_LISTEN_ADDR", None)], Config::from_env).unwrap();
        assert_eq!(cfg.webdav_listen_addr.to_string(), "0.0.0.0:8081");
    }

    #[test]
    fn test_custom_webdav_listen_addr() {
        let cfg = run_with_env(
            &[("NOTEDTHAT_WEBDAV_LISTEN_ADDR", Some("127.0.0.1:9999"))],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.webdav_listen_addr.to_string(), "127.0.0.1:9999");
    }

    #[test]
    fn test_invalid_webdav_listen_addr() {
        let result = run_with_env(
            &[("NOTEDTHAT_WEBDAV_LISTEN_ADDR", Some("not-an-addr"))],
            Config::from_env,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("NOTEDTHAT_WEBDAV_LISTEN_ADDR is invalid")
        );
    }

    #[test]
    fn test_webdav_credentials_propagated() {
        let cfg = run_with_env(
            &[
                ("NOTEDTHAT_WEBDAV_USERNAME", Some("myuser")),
                ("NOTEDTHAT_WEBDAV_PASSWORD", Some("mypass")),
            ],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.webdav_username, "myuser");
        assert_eq!(cfg.webdav_password, "mypass");
    }

    #[test]
    fn all_env_keys_are_accounted_for() {
        assert_eq!(ALL_ENV_KEYS.len(), 26);
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

    mod mcp_http {
        use super::*;

        #[test]
        fn mcp_http_defaults() {
            let cfg = run_with_env(&[], Config::from_env).unwrap();
            assert_eq!(cfg.mcp_http_bind.to_string(), "0.0.0.0:8082");
            assert!(cfg.mcp_http_enabled);
            assert_eq!(cfg.mcp_http_allowed_origins, vec!["null"]);
            assert_eq!(
                cfg.mcp_http_allowed_hosts,
                vec!["127.0.0.1", "localhost", "::1"]
            );
        }

        #[test]
        fn mcp_http_enabled_false() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ENABLED", Some("false"))],
                Config::from_env,
            )
            .unwrap();
            assert!(!cfg.mcp_http_enabled);
        }

        #[test]
        fn mcp_http_enabled_zero() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ENABLED", Some("0"))],
                Config::from_env,
            )
            .unwrap();
            assert!(!cfg.mcp_http_enabled);
        }

        #[test]
        fn mcp_http_enabled_true_by_default() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ENABLED", Some("true"))],
                Config::from_env,
            )
            .unwrap();
            assert!(cfg.mcp_http_enabled);
        }

        #[test]
        fn mcp_http_enabled_any_other_value_is_true() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ENABLED", Some("anything"))],
                Config::from_env,
            )
            .unwrap();
            assert!(cfg.mcp_http_enabled);
        }

        #[test]
        fn mcp_http_custom_bind_addr() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_BIND", Some("127.0.0.1:9999"))],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_bind.to_string(), "127.0.0.1:9999");
        }

        #[test]
        fn mcp_http_invalid_bind_addr() {
            let result = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_BIND", Some("not-an-addr"))],
                Config::from_env,
            );
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("NOTEDTHAT_MCP_HTTP_BIND is invalid")
            );
        }

        #[test]
        fn mcp_http_empty_origins_defaults_to_null() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS", Some(""))],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_allowed_origins, vec!["null"]);
        }

        #[test]
        fn mcp_http_whitespace_origins_defaults_to_null() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS", Some("   "))],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_allowed_origins, vec!["null"]);
        }

        #[test]
        fn mcp_http_single_origin() {
            let cfg = run_with_env(
                &[(
                    "NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS",
                    Some("https://example.com"),
                )],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_allowed_origins, vec!["https://example.com"]);
        }

        #[test]
        fn mcp_http_multiple_origins_comma_separated() {
            let cfg = run_with_env(
                &[(
                    "NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS",
                    Some("https://example.com,https://other.com"),
                )],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(
                cfg.mcp_http_allowed_origins,
                vec!["https://example.com", "https://other.com"]
            );
        }

        #[test]
        fn mcp_http_origins_with_whitespace_trimmed() {
            let cfg = run_with_env(
                &[(
                    "NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS",
                    Some("  https://example.com  ,  https://other.com  "),
                )],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(
                cfg.mcp_http_allowed_origins,
                vec!["https://example.com", "https://other.com"]
            );
        }

        #[test]
        fn mcp_http_empty_hosts_defaults_to_loopback() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS", Some(""))],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(
                cfg.mcp_http_allowed_hosts,
                vec!["127.0.0.1", "localhost", "::1"]
            );
        }

        #[test]
        fn mcp_http_whitespace_hosts_defaults_to_loopback() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS", Some("   "))],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(
                cfg.mcp_http_allowed_hosts,
                vec!["127.0.0.1", "localhost", "::1"]
            );
        }

        #[test]
        fn mcp_http_single_host() {
            let cfg = run_with_env(
                &[("NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS", Some("example.com"))],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_allowed_hosts, vec!["example.com"]);
        }

        #[test]
        fn mcp_http_multiple_hosts_comma_separated() {
            let cfg = run_with_env(
                &[(
                    "NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS",
                    Some("example.com,other.com"),
                )],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_allowed_hosts, vec!["example.com", "other.com"]);
        }

        #[test]
        fn mcp_http_hosts_with_whitespace_trimmed() {
            let cfg = run_with_env(
                &[(
                    "NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS",
                    Some("  example.com  ,  other.com  "),
                )],
                Config::from_env,
            )
            .unwrap();
            assert_eq!(cfg.mcp_http_allowed_hosts, vec!["example.com", "other.com"]);
        }

        #[test]
        fn mcp_http_enabled_with_empty_token_fails() {
            let result = run_with_env(
                &[
                    ("NOTEDTHAT_MCP_HTTP_ENABLED", Some("true")),
                    ("NOTEDTHAT_API_TOKEN", Some("")),
                ],
                Config::from_env,
            );
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("NOTEDTHAT_API_TOKEN"),
                "error should mention NOTEDTHAT_API_TOKEN: {msg}"
            );
        }

        #[test]
        fn mcp_http_enabled_with_whitespace_token_fails() {
            let result = run_with_env(
                &[
                    ("NOTEDTHAT_MCP_HTTP_ENABLED", Some("true")),
                    ("NOTEDTHAT_API_TOKEN", Some("   ")),
                ],
                Config::from_env,
            );
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("NOTEDTHAT_API_TOKEN"),
                "error should mention NOTEDTHAT_API_TOKEN: {msg}"
            );
        }

        #[test]
        fn mcp_http_disabled_with_empty_token_succeeds() {
            let cfg = run_with_env(
                &[
                    ("NOTEDTHAT_MCP_HTTP_ENABLED", Some("false")),
                    ("NOTEDTHAT_API_TOKEN", Some("test-token")),
                ],
                Config::from_env,
            )
            .unwrap();
            assert!(!cfg.mcp_http_enabled);
        }
    }
}
