//! S3 client configuration parsed from `NOTEDTHAT_S3_*` environment variables.

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use notedthat_core::{Error, setting};

/// Every environment variable this backend reads.
///
/// `notedthat-server` uses this to reject variables belonging to the backend that is not
/// selected, so the list must stay in step with [`S3Config::from_env`].
pub const S3_ENV_VARS: [&str; 7] = [
    "NOTEDTHAT_S3_REGION",
    "NOTEDTHAT_S3_ACCESS_KEY_ID",
    "NOTEDTHAT_S3_SECRET_ACCESS_KEY",
    "NOTEDTHAT_S3_ENDPOINT_URL",
    "NOTEDTHAT_S3_FORCE_PATH_STYLE",
    S3_RECONCILE_ENV,
    S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV,
];

/// Whether every knowledge base's bucket is compared against the search index once at
/// startup (D67). `"true"` or `"false"`; default `"true"`.
pub const S3_RECONCILE_ENV: &str = "NOTEDTHAT_S3_RECONCILE";

/// Whether the server may start on a bucket that does not enforce `If-Match` and
/// `If-None-Match` on a `PUT` (D70). `"true"` or `"false"`; default `"false"`, which
/// refuses startup on such a backend.
pub const S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV: &str =
    "NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES";

/// A switch that is exactly `true` or `false`, the way the `fs` backend's are: a value
/// that is neither is a configuration mistake to report, not a `false` to guess.
fn parse_bool(var: &str, supplied: Option<&str>, default: bool) -> Result<bool, Error> {
    match supplied.map(str::trim) {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(Error::Config {
            message: format!(
                "{} is invalid: expected \"true\" or \"false\", got \"{value}\"",
                setting(var)
            ),
        }),
    }
}

/// S3 client config parsed from `NOTEDTHAT_S3_*` env vars.
///
/// All fields are read directly from the process environment; no credential
/// chain (no `~/.aws/credentials` file discovery) is used.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Custom S3-compatible endpoint URL (e.g. `SeaweedFS`, `MinIO`).
    /// If `None`, the standard AWS endpoint is used.
    pub endpoint_url: Option<String>,
    /// AWS region (e.g. `us-east-1`). Required.
    pub region: String,
    /// AWS access key ID. Required.
    pub access_key_id: String,
    /// AWS secret access key. Required.
    pub secret_access_key: String,
    /// Whether to use path-style addressing (required for SeaweedFS/MinIO/Ceph).
    /// Default: `false`.
    pub force_path_style: bool,
    /// Whether to compare every knowledge base's bucket against the search index once
    /// at startup, re-indexing what changed out of band (D67). Default: `true`. The
    /// on-demand pass (`POST …/index/reconcile`) is available either way.
    pub reconcile_on_startup: bool,
    /// Whether a bucket found at startup not to enforce the preconditions on a `PUT`
    /// is accepted — logged, reported `degraded` by `/readyz` and counted in the
    /// metrics — rather than refusing startup (D70). Default: `false`.
    pub allow_unenforced_conditional_writes: bool,
}

/// The raw, unvalidated value of every setting this backend reads.
///
/// One field per entry in [`S3_ENV_VARS`], in the same order. Holding the values
/// before they are checked is what lets one validator serve both configuration
/// sources: [`S3Settings::from_env`] fills this from the environment, and a
/// caller with command-line arguments fills it from those instead. `None` means
/// the setting was not supplied at all — an empty string is a supplied value.
#[derive(Debug, Clone, Default)]
pub struct S3Settings {
    /// `NOTEDTHAT_S3_REGION`.
    pub region: Option<String>,
    /// `NOTEDTHAT_S3_ACCESS_KEY_ID`.
    pub access_key_id: Option<String>,
    /// `NOTEDTHAT_S3_SECRET_ACCESS_KEY`.
    pub secret_access_key: Option<String>,
    /// `NOTEDTHAT_S3_ENDPOINT_URL`.
    pub endpoint_url: Option<String>,
    /// `NOTEDTHAT_S3_FORCE_PATH_STYLE`.
    pub force_path_style: Option<String>,
    /// `NOTEDTHAT_S3_RECONCILE`.
    pub reconcile: Option<String>,
    /// `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES`.
    pub allow_unenforced_conditional_writes: Option<String>,
}

impl S3Settings {
    /// Collect every setting from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            region: std::env::var("NOTEDTHAT_S3_REGION").ok(),
            access_key_id: std::env::var("NOTEDTHAT_S3_ACCESS_KEY_ID").ok(),
            secret_access_key: std::env::var("NOTEDTHAT_S3_SECRET_ACCESS_KEY").ok(),
            endpoint_url: std::env::var("NOTEDTHAT_S3_ENDPOINT_URL").ok(),
            force_path_style: std::env::var("NOTEDTHAT_S3_FORCE_PATH_STYLE").ok(),
            reconcile: std::env::var(S3_RECONCILE_ENV).ok(),
            allow_unenforced_conditional_writes: std::env::var(
                S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV,
            )
            .ok(),
        }
    }
}

impl S3Config {
    /// Parse S3 configuration from environment variables.
    ///
    /// # Required environment variables
    /// - `NOTEDTHAT_S3_REGION`
    /// - `NOTEDTHAT_S3_ACCESS_KEY_ID`
    /// - `NOTEDTHAT_S3_SECRET_ACCESS_KEY`
    ///
    /// # Optional environment variables
    /// - `NOTEDTHAT_S3_ENDPOINT_URL` — defaults to AWS endpoint
    /// - `NOTEDTHAT_S3_FORCE_PATH_STYLE` — `true` or `false`, defaults to `false`
    /// - `NOTEDTHAT_S3_RECONCILE` — `true` or `false`, defaults to `true`
    /// - `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES` — `true` or `false`, defaults
    ///   to `false`
    pub fn from_env() -> Result<Self, Error> {
        Self::from_settings(S3Settings::from_env())
    }

    /// Validate already-collected settings, whatever supplied them.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` when a required setting is absent, or when
    /// `NOTEDTHAT_S3_FORCE_PATH_STYLE`, `NOTEDTHAT_S3_RECONCILE` or
    /// `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES` is neither `true` nor `false`.
    pub fn from_settings(settings: S3Settings) -> Result<Self, Error> {
        let region = settings.region.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_S3_REGION")),
        })?;
        let access_key_id = settings.access_key_id.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_S3_ACCESS_KEY_ID")),
        })?;
        let secret_access_key = settings.secret_access_key.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_S3_SECRET_ACCESS_KEY")),
        })?;
        let force_path_style = parse_bool(
            "NOTEDTHAT_S3_FORCE_PATH_STYLE",
            settings.force_path_style.as_deref(),
            false,
        )?;
        let reconcile_on_startup =
            parse_bool(S3_RECONCILE_ENV, settings.reconcile.as_deref(), true)?;
        let allow_unenforced_conditional_writes = parse_bool(
            S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV,
            settings.allow_unenforced_conditional_writes.as_deref(),
            false,
        )?;

        Ok(Self {
            endpoint_url: settings.endpoint_url,
            region,
            access_key_id,
            secret_access_key,
            force_path_style,
            reconcile_on_startup,
            allow_unenforced_conditional_writes,
        })
    }

    /// Build an [`aws_sdk_s3::Client`] from this configuration.
    ///
    /// Credentials are supplied directly (static provider); no ambient
    /// credential chain is consulted.
    #[must_use]
    pub fn build_client(&self) -> Client {
        let creds = Credentials::new(
            &self.access_key_id,
            &self.secret_access_key,
            None,
            None,
            "notedthat-static",
        );
        let mut builder = aws_sdk_s3::config::Builder::new()
            .region(Region::new(self.region.clone()))
            .credentials_provider(creds)
            .force_path_style(self.force_path_style)
            .behavior_version(BehaviorVersion::latest());
        if let Some(url) = &self.endpoint_url {
            builder = builder.endpoint_url(url);
        }
        Client::from_conf(builder.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_env_all_vars() {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
                ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("test-key")),
                ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("test-secret")),
                ("NOTEDTHAT_S3_ENDPOINT_URL", Some("http://localhost:8333")),
                ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
            ],
            || {
                let cfg = S3Config::from_env().expect("should parse");
                assert_eq!(cfg.region, "us-east-1");
                assert_eq!(cfg.endpoint_url, Some("http://localhost:8333".into()));
                assert!(!cfg.force_path_style);
            },
        );
    }

    #[test]
    fn test_from_env_missing_region() {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_S3_REGION", None),
                ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("key")),
                ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("secret")),
                ("NOTEDTHAT_S3_ENDPOINT_URL", None),
                ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
            ],
            || {
                let result = S3Config::from_env();
                assert!(result.is_err(), "missing region should fail");
                let err_msg = result.unwrap_err().to_string();
                assert!(
                    err_msg.contains("NOTEDTHAT_S3_REGION"),
                    "error should mention the missing var"
                );
            },
        );
    }

    #[test]
    fn test_from_env_force_path_style_true() {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
                ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("test-key")),
                ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("test-secret")),
                ("NOTEDTHAT_S3_ENDPOINT_URL", None),
                ("NOTEDTHAT_S3_FORCE_PATH_STYLE", Some("true")),
            ],
            || {
                let cfg = S3Config::from_env().expect("should parse");
                assert!(cfg.force_path_style);
            },
        );
    }

    fn with_reconcile(value: Option<&str>, check: impl FnOnce(Result<S3Config, Error>)) {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
                ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("test-key")),
                ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("test-secret")),
                ("NOTEDTHAT_S3_ENDPOINT_URL", None),
                ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
                (S3_RECONCILE_ENV, value),
            ],
            || check(S3Config::from_env()),
        );
    }

    #[test]
    fn reconcile_defaults_to_on() {
        with_reconcile(None, |cfg| {
            assert!(cfg.expect("parses").reconcile_on_startup);
        });
    }

    #[test]
    fn reconcile_can_be_switched_off() {
        with_reconcile(Some("false"), |cfg| {
            assert!(!cfg.expect("parses").reconcile_on_startup);
        });
        with_reconcile(Some(" true "), |cfg| {
            assert!(cfg.expect("parses").reconcile_on_startup);
        });
    }

    #[test]
    fn reconcile_refuses_anything_but_true_or_false() {
        for value in ["yes", "1", "", "TRUE"] {
            with_reconcile(Some(value), |cfg| {
                let error = cfg.expect_err("refused").to_string();
                assert!(
                    error.contains("NOTEDTHAT_S3_RECONCILE")
                        && error.contains("--s3-reconcile")
                        && error.contains("expected \"true\" or \"false\""),
                    "{value:?}: {error}"
                );
            });
        }
    }

    fn with_allow_unenforced(value: Option<&str>, check: impl FnOnce(Result<S3Config, Error>)) {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
                ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("test-key")),
                ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("test-secret")),
                ("NOTEDTHAT_S3_ENDPOINT_URL", None),
                ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
                (S3_RECONCILE_ENV, None),
                (S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV, value),
            ],
            || check(S3Config::from_env()),
        );
    }

    #[test]
    fn unenforced_conditional_writes_are_refused_by_default() {
        with_allow_unenforced(None, |cfg| {
            assert!(!cfg.expect("parses").allow_unenforced_conditional_writes);
        });
    }

    #[test]
    fn unenforced_conditional_writes_can_be_allowed() {
        with_allow_unenforced(Some("true"), |cfg| {
            assert!(cfg.expect("parses").allow_unenforced_conditional_writes);
        });
        with_allow_unenforced(Some(" false "), |cfg| {
            assert!(!cfg.expect("parses").allow_unenforced_conditional_writes);
        });
    }

    #[test]
    fn allow_unenforced_refuses_anything_but_true_or_false() {
        for value in ["yes", "1", "", "TRUE"] {
            with_allow_unenforced(Some(value), |cfg| {
                let error = cfg.expect_err("refused").to_string();
                assert!(
                    error.contains("NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES")
                        && error.contains("--s3-allow-unenforced-conditional-writes")
                        && error.contains("expected \"true\" or \"false\""),
                    "{value:?}: {error}"
                );
            });
        }
    }

    /// The one a `SeaweedFS`, `MinIO`, Ceph or Garage deployment cannot afford to have
    /// guessed: `yes` used to parse as `false`, and every request then went out
    /// virtual-host style with nothing in the logs naming the setting that caused it.
    #[test]
    fn force_path_style_refuses_anything_but_true_or_false() {
        for value in ["yes", "1", "on", "True"] {
            temp_env::with_vars(
                [
                    ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
                    ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("test-key")),
                    ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("test-secret")),
                    ("NOTEDTHAT_S3_ENDPOINT_URL", None),
                    ("NOTEDTHAT_S3_FORCE_PATH_STYLE", Some(value)),
                    (S3_RECONCILE_ENV, None),
                ],
                || {
                    let error = S3Config::from_env().expect_err("refused").to_string();
                    assert!(
                        error.contains("NOTEDTHAT_S3_FORCE_PATH_STYLE")
                            && error.contains("--s3-force-path-style")
                            && error.contains("expected \"true\" or \"false\""),
                        "{value:?}: {error}"
                    );
                },
            );
        }
    }

    #[test]
    fn test_from_env_force_path_style_default_false() {
        temp_env::with_vars(
            [
                ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
                ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("test-key")),
                ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("test-secret")),
                ("NOTEDTHAT_S3_ENDPOINT_URL", None),
                ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
            ],
            || {
                let cfg = S3Config::from_env().expect("should parse");
                assert!(!cfg.force_path_style);
            },
        );
    }
}
