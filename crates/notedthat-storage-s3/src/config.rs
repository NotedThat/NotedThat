//! S3 client configuration parsed from `NOTEDTHAT_S3_*` environment variables.

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use notedthat_core::Error;

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
    pub fn from_env() -> Result<Self, Error> {
        let region = std::env::var("NOTEDTHAT_S3_REGION").map_err(|_| Error::Config {
            message: "NOTEDTHAT_S3_REGION is required".into(),
        })?;
        let access_key_id =
            std::env::var("NOTEDTHAT_S3_ACCESS_KEY_ID").map_err(|_| Error::Config {
                message: "NOTEDTHAT_S3_ACCESS_KEY_ID is required".into(),
            })?;
        let secret_access_key =
            std::env::var("NOTEDTHAT_S3_SECRET_ACCESS_KEY").map_err(|_| Error::Config {
                message: "NOTEDTHAT_S3_SECRET_ACCESS_KEY is required".into(),
            })?;
        let endpoint_url = std::env::var("NOTEDTHAT_S3_ENDPOINT_URL").ok();
        let force_path_style = std::env::var("NOTEDTHAT_S3_FORCE_PATH_STYLE")
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false);

        Ok(Self {
            endpoint_url,
            region,
            access_key_id,
            secret_access_key,
            force_path_style,
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
