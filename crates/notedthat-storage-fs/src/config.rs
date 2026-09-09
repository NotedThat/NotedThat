//! Configuration for the filesystem backend, parsed from `NOTEDTHAT_FS_*`.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use notedthat_core::Error;

/// Root directory holding every knowledge base's objects.
pub const FS_ROOT_ENV: &str = "NOTEDTHAT_FS_ROOT";
/// Where per-object metadata is kept.
pub const FS_METADATA_ENV: &str = "NOTEDTHAT_FS_METADATA";
/// Mode bits for created object files.
pub const FS_FILE_MODE_ENV: &str = "NOTEDTHAT_FS_FILE_MODE";
/// Mode bits for created directories.
pub const FS_DIR_MODE_ENV: &str = "NOTEDTHAT_FS_DIR_MODE";
/// Opt out of the case-sensitivity and Unicode-normalization startup checks.
pub const FS_ALLOW_LOSSY_NAMES_ENV: &str = "NOTEDTHAT_FS_ALLOW_LOSSY_NAMES";

/// Every environment variable this backend reads.
///
/// `notedthat-server` uses this to reject variables belonging to the backend that is not
/// selected, so the list must stay complete.
pub const FS_ENV_VARS: [&str; 5] = [
    FS_ROOT_ENV,
    FS_METADATA_ENV,
    FS_FILE_MODE_ENV,
    FS_DIR_MODE_ENV,
    FS_ALLOW_LOSSY_NAMES_ENV,
];

/// Where per-object metadata (`ETag`, content type) is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataMode {
    /// A JSON file in a shadow tree beside the objects.
    ///
    /// Survives `cp`, `rsync` and `tar` with no special flags, and — importantly for a
    /// tree people edit by hand — survives the write-new-file-then-rename dance that
    /// most editors perform on save.
    #[default]
    Sidecar,
}

impl MetadataMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "sidecar" => Some(Self::Sidecar),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sidecar => "sidecar",
        }
    }
}

impl std::fmt::Display for MetadataMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Filesystem backend configuration.
#[derive(Debug, Clone)]
pub struct FsConfig {
    /// Absolute path of the storage root. No default: a default would silently create a
    /// data directory the operator did not choose, and getting that wrong means writing
    /// production data outside the backup set.
    pub root: PathBuf,
    /// Where per-object metadata is kept.
    pub metadata: MetadataMode,
    /// Mode bits for created object files. `tempfile` creates at `0600`; committing that
    /// into a tree meant to be browsable would make every note unreadable to anyone but
    /// the server user.
    pub file_mode: u32,
    /// Mode bits for created directories.
    pub dir_mode: u32,
    /// Start even when the root's filesystem folds case or normalizes Unicode.
    pub allow_lossy_names: bool,
}

/// The raw, unvalidated value of every setting this backend reads.
///
/// One field per entry in [`FS_ENV_VARS`], in the same order. Holding the values
/// before they are checked is what lets one validator serve both configuration
/// sources: [`FsSettings::from_env`] fills this from the environment, and a
/// caller with command-line arguments fills it from those instead. `None` means
/// the setting was not supplied at all — an empty value is a supplied value.
///
/// `OsString` rather than `String` throughout, because the root is read with
/// `var_os` today and a path is not obliged to be UTF-8; the enumerated settings
/// keep their own "must be valid UTF-8" error rather than losing the value here.
#[derive(Debug, Clone, Default)]
pub struct FsSettings {
    /// [`FS_ROOT_ENV`].
    pub root: Option<OsString>,
    /// [`FS_METADATA_ENV`].
    pub metadata: Option<OsString>,
    /// [`FS_FILE_MODE_ENV`].
    pub file_mode: Option<OsString>,
    /// [`FS_DIR_MODE_ENV`].
    pub dir_mode: Option<OsString>,
    /// [`FS_ALLOW_LOSSY_NAMES_ENV`].
    pub allow_lossy_names: Option<OsString>,
}

impl FsSettings {
    /// Collect every setting from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            root: std::env::var_os(FS_ROOT_ENV),
            metadata: std::env::var_os(FS_METADATA_ENV),
            file_mode: std::env::var_os(FS_FILE_MODE_ENV),
            dir_mode: std::env::var_os(FS_DIR_MODE_ENV),
            allow_lossy_names: std::env::var_os(FS_ALLOW_LOSSY_NAMES_ENV),
        }
    }
}

impl FsConfig {
    /// Parse the configuration from the environment.
    ///
    /// # Errors
    ///
    /// See [`FsConfig::from_settings`].
    pub fn from_env() -> Result<Self, Error> {
        Self::from_settings(FsSettings::from_env())
    }

    /// Validate already-collected settings, whatever supplied them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the root is missing, empty or relative, or when an
    /// enumerated value is not one this build accepts. Parsing is strict: unlike
    /// `NOTEDTHAT_LOG_FORMAT`, a typo here would silently change where an operator's
    /// bytes live, and nothing later in the run would say so.
    pub fn from_settings(settings: FsSettings) -> Result<Self, Error> {
        let root = match settings.root {
            None => {
                return Err(config_error(format!(
                    "{FS_ROOT_ENV} is required when NOTEDTHAT_STORAGE_BACKEND=fs"
                )));
            }
            Some(value) if value.is_empty() => {
                return Err(config_error(format!("{FS_ROOT_ENV} must not be empty")));
            }
            Some(value) => PathBuf::from(value),
        };

        if !root.is_absolute() {
            // A relative root resolves against the process working directory, which
            // differs between systemd, `docker run` and a shell — so one config would
            // mean three directories.
            return Err(config_error(format!(
                "{FS_ROOT_ENV} must be an absolute path, got '{}'",
                root.display()
            )));
        }

        let metadata = parse_enum(
            FS_METADATA_ENV,
            settings.metadata.as_deref(),
            MetadataMode::parse,
            MetadataMode::default(),
            "\"sidecar\"",
        )?;
        let file_mode = parse_mode(FS_FILE_MODE_ENV, settings.file_mode.as_deref(), 0o644)?;
        let dir_mode = parse_mode(FS_DIR_MODE_ENV, settings.dir_mode.as_deref(), 0o755)?;
        let allow_lossy_names = parse_bool(
            FS_ALLOW_LOSSY_NAMES_ENV,
            settings.allow_lossy_names.as_deref(),
            false,
        )?;

        Ok(Self {
            root,
            metadata,
            file_mode,
            dir_mode,
            allow_lossy_names,
        })
    }

    /// Build a configuration directly, for tests and embedders.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            metadata: MetadataMode::default(),
            file_mode: 0o644,
            dir_mode: 0o755,
            allow_lossy_names: false,
        }
    }
}

fn parse_enum<T>(
    var: &str,
    supplied: Option<&OsStr>,
    parse: impl Fn(&str) -> Option<T>,
    default: T,
    accepted: &str,
) -> Result<T, Error> {
    match supplied {
        None => Ok(default),
        Some(value) if value.is_empty() => Err(config_error(format!("{var} must not be empty"))),
        Some(value) => {
            let value = value
                .to_str()
                .ok_or_else(|| config_error(format!("{var} must be valid UTF-8")))?;
            parse(value).ok_or_else(|| {
                config_error(format!(
                    "{var} is invalid: expected {accepted}, got \"{value}\""
                ))
            })
        }
    }
}

fn parse_mode(var: &str, supplied: Option<&OsStr>, default: u32) -> Result<u32, Error> {
    let Some(value) = supplied else {
        return Ok(default);
    };
    let value = value
        .to_str()
        .ok_or_else(|| config_error(format!("{var} must be valid UTF-8")))?;
    if value.is_empty() {
        return Err(config_error(format!("{var} must not be empty")));
    }
    let digits = value.strip_prefix("0o").unwrap_or(value);
    u32::from_str_radix(digits, 8)
        .ok()
        .filter(|mode| *mode <= 0o7777)
        .ok_or_else(|| {
            config_error(format!(
                "{var} is invalid: expected octal mode bits such as 0644, got \"{value}\""
            ))
        })
}

fn parse_bool(var: &str, supplied: Option<&OsStr>, default: bool) -> Result<bool, Error> {
    parse_enum(
        var,
        supplied,
        |value| match value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        default,
        "\"true\" or \"false\"",
    )
}

fn config_error(message: String) -> Error {
    Error::Config { message }
}

#[cfg(test)]
mod tests {
    use super::{FsConfig, MetadataMode};

    fn with<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let baseline = [
            ("NOTEDTHAT_FS_ROOT", None),
            ("NOTEDTHAT_FS_METADATA", None),
            ("NOTEDTHAT_FS_FILE_MODE", None),
            ("NOTEDTHAT_FS_DIR_MODE", None),
            ("NOTEDTHAT_FS_ALLOW_LOSSY_NAMES", None),
        ];
        let mut merged: Vec<(&str, Option<&str>)> = baseline.to_vec();
        for (key, value) in vars {
            if let Some(slot) = merged.iter_mut().find(|(name, _)| name == key) {
                slot.1 = *value;
            } else {
                merged.push((key, *value));
            }
        }
        temp_env::with_vars(merged, f)
    }

    #[test]
    fn root_is_required() {
        with(&[], || {
            let error = FsConfig::from_env().unwrap_err().to_string();
            assert!(error.contains("NOTEDTHAT_FS_ROOT is required"), "{error}");
        });
    }

    #[test]
    fn root_must_be_absolute() {
        with(&[("NOTEDTHAT_FS_ROOT", Some("relative/path"))], || {
            let error = FsConfig::from_env().unwrap_err().to_string();
            assert!(error.contains("must be an absolute path"), "{error}");
        });
    }

    #[test]
    fn root_must_not_be_empty() {
        with(&[("NOTEDTHAT_FS_ROOT", Some(""))], || {
            let error = FsConfig::from_env().unwrap_err().to_string();
            assert!(error.contains("must not be empty"), "{error}");
        });
    }

    #[test]
    fn defaults_are_sidecar_and_browsable_modes() {
        with(&[("NOTEDTHAT_FS_ROOT", Some("/srv/nt"))], || {
            let config = FsConfig::from_env().expect("valid");
            assert_eq!(config.metadata, MetadataMode::Sidecar);
            assert_eq!(config.file_mode, 0o644);
            assert_eq!(config.dir_mode, 0o755);
            assert!(!config.allow_lossy_names);
        });
    }

    #[test]
    fn an_unknown_metadata_mode_is_refused_rather_than_defaulted() {
        with(
            &[
                ("NOTEDTHAT_FS_ROOT", Some("/srv/nt")),
                ("NOTEDTHAT_FS_METADATA", Some("xattr")),
            ],
            || {
                let error = FsConfig::from_env().unwrap_err().to_string();
                assert!(error.contains("expected \"sidecar\""), "{error}");
            },
        );
    }

    #[test]
    fn mode_bits_accept_octal_with_or_without_a_prefix() {
        with(
            &[
                ("NOTEDTHAT_FS_ROOT", Some("/srv/nt")),
                ("NOTEDTHAT_FS_FILE_MODE", Some("0640")),
                ("NOTEDTHAT_FS_DIR_MODE", Some("0o750")),
            ],
            || {
                let config = FsConfig::from_env().expect("valid");
                assert_eq!(config.file_mode, 0o640);
                assert_eq!(config.dir_mode, 0o750);
            },
        );
    }

    #[test]
    fn a_non_octal_mode_is_refused() {
        with(
            &[
                ("NOTEDTHAT_FS_ROOT", Some("/srv/nt")),
                ("NOTEDTHAT_FS_FILE_MODE", Some("rw-r--r--")),
            ],
            || {
                let error = FsConfig::from_env().unwrap_err().to_string();
                assert!(error.contains("octal mode bits"), "{error}");
            },
        );
    }
}
