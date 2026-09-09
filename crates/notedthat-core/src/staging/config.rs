//! Staging configuration and typed failures.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Environment variable selecting the directory used for staged upload files.
pub const UPLOAD_TMP_DIR_ENV: &str = "NOTEDTHAT_UPLOAD_TMP_DIR";

/// A thread-safe boxed source error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Configuration for private staging files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagingConfig {
    pub(super) directory: PathBuf,
}

impl StagingConfig {
    /// Use an explicit staging directory.
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Read the optional staging directory override from the environment.
    pub fn from_env() -> Result<Self, StageError> {
        Self::from_setting(std::env::var_os(UPLOAD_TMP_DIR_ENV))
    }

    /// Build from an already-resolved staging directory override.
    ///
    /// The value arrives here from whichever source supplied it — a command-line
    /// argument or [`UPLOAD_TMP_DIR_ENV`] — so the "set but empty" rule is
    /// enforced in one place for both.
    pub fn from_setting(directory: Option<OsString>) -> Result<Self, StageError> {
        match directory {
            Some(value) if value.is_empty() => Err(StageError::Config {
                message: format!("{} must not be empty", crate::setting(UPLOAD_TMP_DIR_ENV)),
            }),
            Some(value) => Ok(Self::new(PathBuf::from(value))),
            None => Ok(Self::default()),
        }
    }

    /// Directory in which private staging files are created.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Verify that the directory exists and accepts private temporary files.
    pub async fn validate(&self) -> Result<(), StageError> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            let mut probe = tempfile::tempfile_in(directory)?;
            probe.write_all(b"notedthat-staging-probe")?;
            probe.flush()?;
            probe.sync_all()
        })
        .await
        .map_err(|error| StageError::Config {
            message: format!("staging directory validation task failed: {error}"),
        })?
        .map_err(|error| StageError::Config {
            message: format!("staging directory is unusable: {error}"),
        })
    }
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self::new(std::env::temp_dir())
    }
}

/// A failure while receiving or persisting a staged body.
#[derive(Debug, Error)]
pub enum StageError {
    /// More bytes arrived than the configured limit permits.
    #[error("body is {size} bytes, exceeding limit {limit}")]
    TooLarge {
        /// Observed or declared byte count.
        size: u64,
        /// Maximum accepted byte count.
        limit: u64,
    },
    /// The received byte count differs from the declared byte count.
    #[error("declared body length {expected} differs from received length {actual}")]
    LengthMismatch {
        /// Declared byte count.
        expected: u64,
        /// Observed byte count.
        actual: u64,
    },
    /// Reading the source body failed.
    #[error("reading request body failed: {source}")]
    Read {
        /// Source reader failure.
        source: BoxError,
    },
    /// Creating or writing a private staging file failed.
    #[error("writing staged body failed: {source}")]
    Write {
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Staging configuration is invalid or unusable.
    #[error("staging configuration error: {message}")]
    Config {
        /// Actionable configuration failure detail.
        message: String,
    },
}
