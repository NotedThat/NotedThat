//! S3 client configuration parsed from `NOTEDTHAT_S3_*` environment variables.

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use notedthat_core::{Error, setting};

/// AWS region, e.g. `us-east-1`. Required.
pub const S3_REGION_ENV: &str = "NOTEDTHAT_S3_REGION";
/// AWS access key ID. Required.
pub const S3_ACCESS_KEY_ID_ENV: &str = "NOTEDTHAT_S3_ACCESS_KEY_ID";
/// AWS secret access key. Required.
pub const S3_SECRET_ACCESS_KEY_ENV: &str = "NOTEDTHAT_S3_SECRET_ACCESS_KEY";
/// Custom S3-compatible endpoint URL; the standard AWS endpoint when unset.
pub const S3_ENDPOINT_URL_ENV: &str = "NOTEDTHAT_S3_ENDPOINT_URL";
/// Path-style addressing, which `SeaweedFS`, `MinIO` and Ceph need. `"true"` or
/// `"false"`; default `"false"`.
pub const S3_FORCE_PATH_STYLE_ENV: &str = "NOTEDTHAT_S3_FORCE_PATH_STYLE";
/// Whether every knowledge base's bucket is compared against the search index once at
/// startup (D67). `"true"` or `"false"`; default `"true"`.
pub const S3_RECONCILE_ENV: &str = "NOTEDTHAT_S3_RECONCILE";
/// Whether the server may start on a bucket that does not enforce `If-Match` and
/// `If-None-Match` on a `PUT` (D70). `"true"` or `"false"`; default `"false"`, which
/// refuses startup on such a backend.
pub const S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV: &str =
    "NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES";

/// Every environment variable this backend reads.
///
/// `notedthat-server` uses this to reject variables belonging to the backend that is not
/// selected, so the list must stay in step with [`S3Settings`].
pub const S3_ENV_VARS: [&str; 7] = [
    S3_REGION_ENV,
    S3_ACCESS_KEY_ID_ENV,
    S3_SECRET_ACCESS_KEY_ENV,
    S3_ENDPOINT_URL_ENV,
    S3_FORCE_PATH_STYLE_ENV,
    S3_RECONCILE_ENV,
    S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV,
];

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
/// sources: `notedthat-server` fills this from its command line and the
/// environment. `None` means the setting was not supplied at all — an empty
/// string is a supplied value.
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

impl S3Config {
    /// Validate already-collected settings, whatever supplied them.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` when a required setting is absent, or when
    /// `NOTEDTHAT_S3_FORCE_PATH_STYLE`, `NOTEDTHAT_S3_RECONCILE` or
    /// `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES` is neither `true` nor `false`.
    pub fn from_settings(settings: S3Settings) -> Result<Self, Error> {
        let region = settings.region.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting(S3_REGION_ENV)),
        })?;
        let access_key_id = settings.access_key_id.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting(S3_ACCESS_KEY_ID_ENV)),
        })?;
        let secret_access_key = settings.secret_access_key.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting(S3_SECRET_ACCESS_KEY_ENV)),
        })?;
        let force_path_style = parse_bool(
            S3_FORCE_PATH_STYLE_ENV,
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

    /// The three required settings, and nothing else.
    fn base() -> S3Settings {
        S3Settings {
            region: Some("us-east-1".into()),
            access_key_id: Some("test-key".into()),
            secret_access_key: Some("test-secret".into()),
            ..S3Settings::default()
        }
    }

    #[test]
    fn all_settings_parse() {
        let cfg = S3Config::from_settings(S3Settings {
            endpoint_url: Some("http://localhost:8333".into()),
            ..base()
        })
        .expect("should parse");
        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(cfg.endpoint_url, Some("http://localhost:8333".into()));
        assert!(!cfg.force_path_style);
    }

    #[test]
    fn a_missing_region_is_refused() {
        let result = S3Config::from_settings(S3Settings {
            region: None,
            ..base()
        });
        assert!(result.is_err(), "missing region should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("NOTEDTHAT_S3_REGION"),
            "error should mention the missing var"
        );
    }

    #[test]
    fn force_path_style_true() {
        let cfg = S3Config::from_settings(S3Settings {
            force_path_style: Some("true".into()),
            ..base()
        })
        .expect("should parse");
        assert!(cfg.force_path_style);
    }

    fn with_reconcile(value: Option<&str>) -> Result<S3Config, Error> {
        S3Config::from_settings(S3Settings {
            reconcile: value.map(Into::into),
            ..base()
        })
    }

    #[test]
    fn reconcile_defaults_to_on() {
        assert!(with_reconcile(None).expect("parses").reconcile_on_startup);
    }

    #[test]
    fn reconcile_can_be_switched_off() {
        assert!(
            !with_reconcile(Some("false"))
                .expect("parses")
                .reconcile_on_startup
        );
        assert!(
            with_reconcile(Some(" true "))
                .expect("parses")
                .reconcile_on_startup
        );
    }

    #[test]
    fn reconcile_refuses_anything_but_true_or_false() {
        for value in ["yes", "1", "", "TRUE"] {
            let error = with_reconcile(Some(value))
                .expect_err("refused")
                .to_string();
            assert!(
                error.contains("NOTEDTHAT_S3_RECONCILE")
                    && error.contains("--s3-reconcile")
                    && error.contains("expected \"true\" or \"false\""),
                "{value:?}: {error}"
            );
        }
    }

    fn with_allow_unenforced(value: Option<&str>) -> Result<S3Config, Error> {
        S3Config::from_settings(S3Settings {
            allow_unenforced_conditional_writes: value.map(Into::into),
            ..base()
        })
    }

    #[test]
    fn unenforced_conditional_writes_are_refused_by_default() {
        assert!(
            !with_allow_unenforced(None)
                .expect("parses")
                .allow_unenforced_conditional_writes
        );
    }

    #[test]
    fn unenforced_conditional_writes_can_be_allowed() {
        assert!(
            with_allow_unenforced(Some("true"))
                .expect("parses")
                .allow_unenforced_conditional_writes
        );
        assert!(
            !with_allow_unenforced(Some(" false "))
                .expect("parses")
                .allow_unenforced_conditional_writes
        );
    }

    #[test]
    fn allow_unenforced_refuses_anything_but_true_or_false() {
        for value in ["yes", "1", "", "TRUE"] {
            let error = with_allow_unenforced(Some(value))
                .expect_err("refused")
                .to_string();
            assert!(
                error.contains("NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES")
                    && error.contains("--s3-allow-unenforced-conditional-writes")
                    && error.contains("expected \"true\" or \"false\""),
                "{value:?}: {error}"
            );
        }
    }

    /// The one a `SeaweedFS`, `MinIO`, Ceph or Garage deployment cannot afford to have
    /// guessed: `yes` used to parse as `false`, and every request then went out
    /// virtual-host style with nothing in the logs naming the setting that caused it.
    #[test]
    fn force_path_style_refuses_anything_but_true_or_false() {
        for value in ["yes", "1", "on", "True"] {
            let error = S3Config::from_settings(S3Settings {
                force_path_style: Some(value.into()),
                ..base()
            })
            .expect_err("refused")
            .to_string();
            assert!(
                error.contains("NOTEDTHAT_S3_FORCE_PATH_STYLE")
                    && error.contains("--s3-force-path-style")
                    && error.contains("expected \"true\" or \"false\""),
                "{value:?}: {error}"
            );
        }
    }

    #[test]
    fn force_path_style_defaults_to_false() {
        let cfg = S3Config::from_settings(base()).expect("should parse");
        assert!(!cfg.force_path_style);
    }
}
