//! The NATS connection and stream settings every `NotedThat` `JetStream` user shares.
//!
//! One broker, one connection per process: the events log (D55) and, in a later
//! phase, the indexing queue both read these. Which of them selects NATS is `notedthat-server`'s
//! concern; what lives here is one settings struct and one validator, in the shape
//! of the storage adapters' configs.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;

use notedthat_core::{Error, setting};

/// Every environment variable the NATS connection reads.
pub const NATS_CONNECT_ENV_VARS: [&str; 11] = [
    "NOTEDTHAT_NATS_URL",
    "NOTEDTHAT_NATS_CREDS_FILE",
    "NOTEDTHAT_NATS_NKEY_SEED_FILE",
    "NOTEDTHAT_NATS_TOKEN",
    "NOTEDTHAT_NATS_TLS_CA_FILE",
    "NOTEDTHAT_NATS_TLS_CERT_FILE",
    "NOTEDTHAT_NATS_TLS_KEY_FILE",
    "NOTEDTHAT_NATS_TLS_REQUIRED",
    "NOTEDTHAT_NATS_REPLICAS",
    "NOTEDTHAT_NATS_STORAGE",
    "NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS",
];

/// Replicas of a stream created while `NOTEDTHAT_NATS_REPLICAS` is unset.
pub const DEFAULT_NATS_REPLICAS: usize = 1;
/// The `JetStream` publish deduplication window of a stream created while
/// `NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS` is unset: two minutes, the server's own
/// default, shortened to the stream's retention when that is shorter.
pub const DEFAULT_NATS_DUPLICATE_WINDOW_SECS: u64 = 120;
/// The shortest duplicate window accepted: twice the publish acknowledgement
/// timeout. A publish whose acknowledgement timed out is retried under the same
/// `Nats-Msg-Id` only after waiting that long, and the window counts from when
/// the broker stored the first copy; a shorter window has usually expired by the
/// time the retry arrives, and the broker stores the event a second time.
pub const MIN_NATS_DUPLICATE_WINDOW_SECS: u64 = 2 * crate::connect::TIMEOUT.as_secs();
/// The most replicas `JetStream` accepts for a stream.
const MAX_NATS_REPLICAS: u64 = 5;

/// The raw, unvalidated value of every connection setting.
///
/// `None` means not supplied; an empty string is a supplied value.
#[derive(Debug, Clone, Default)]
pub struct NatsConnectSettings {
    /// `NOTEDTHAT_NATS_URL`.
    pub url: Option<String>,
    /// `NOTEDTHAT_NATS_CREDS_FILE`.
    pub creds_file: Option<OsString>,
    /// `NOTEDTHAT_NATS_NKEY_SEED_FILE`.
    pub nkey_seed_file: Option<OsString>,
    /// `NOTEDTHAT_NATS_TOKEN`.
    pub token: Option<String>,
    /// `NOTEDTHAT_NATS_TLS_CA_FILE`.
    pub tls_ca_file: Option<OsString>,
    /// `NOTEDTHAT_NATS_TLS_CERT_FILE`.
    pub tls_cert_file: Option<OsString>,
    /// `NOTEDTHAT_NATS_TLS_KEY_FILE`.
    pub tls_key_file: Option<OsString>,
    /// `NOTEDTHAT_NATS_TLS_REQUIRED`.
    pub tls_required: Option<OsString>,
    /// `NOTEDTHAT_NATS_REPLICAS`.
    pub replicas: Option<OsString>,
    /// `NOTEDTHAT_NATS_STORAGE`.
    pub storage: Option<OsString>,
    /// `NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS`.
    pub duplicate_window_secs: Option<OsString>,
}

impl NatsConnectSettings {
    /// Collect every setting from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            url: std::env::var("NOTEDTHAT_NATS_URL").ok(),
            creds_file: std::env::var_os("NOTEDTHAT_NATS_CREDS_FILE"),
            nkey_seed_file: std::env::var_os("NOTEDTHAT_NATS_NKEY_SEED_FILE"),
            token: std::env::var("NOTEDTHAT_NATS_TOKEN").ok(),
            tls_ca_file: std::env::var_os("NOTEDTHAT_NATS_TLS_CA_FILE"),
            tls_cert_file: std::env::var_os("NOTEDTHAT_NATS_TLS_CERT_FILE"),
            tls_key_file: std::env::var_os("NOTEDTHAT_NATS_TLS_KEY_FILE"),
            tls_required: std::env::var_os("NOTEDTHAT_NATS_TLS_REQUIRED"),
            replicas: std::env::var_os("NOTEDTHAT_NATS_REPLICAS"),
            storage: std::env::var_os("NOTEDTHAT_NATS_STORAGE"),
            duplicate_window_secs: std::env::var_os("NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS"),
        }
    }
}

/// How the connection authenticates beyond what the URL carries.
///
/// At most one, and none when the URL carries credentials: NATS takes one
/// identity per connection, and two configured ways of proving one would leave
/// the operator guessing which the server used.
#[derive(Clone, PartialEq, Eq)]
pub enum NatsAuth {
    /// Nothing beyond the URL, which may itself carry `user:pass@` or `token@`.
    None,
    /// A decentralised-auth `.creds` file: a user JWT and its `NKey` seed.
    CredsFile(PathBuf),
    /// A file holding an `NKey` seed (`SU…`).
    NkeySeedFile(PathBuf),
    /// A server token.
    Token(String),
}

impl std::fmt::Debug for NatsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::CredsFile(path) => f.debug_tuple("CredsFile").field(path).finish(),
            Self::NkeySeedFile(path) => f.debug_tuple("NkeySeedFile").field(path).finish(),
            // A token is a credential; `{:?}` of the whole config must not print it.
            Self::Token(_) => f.write_str("Token(<redacted>)"),
        }
    }
}

/// TLS towards the broker. Every field is optional; a `tls://` URL alone turns
/// TLS on with the system roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NatsTls {
    /// Root certificates, PEM, trusted *instead of* the system roots: the file
    /// must hold every CA the broker's certificates, and those of any cluster
    /// peer it advertises, chain to.
    pub ca_file: Option<PathBuf>,
    /// A client certificate and its key, PEM, for mutual TLS.
    pub client_cert: Option<(PathBuf, PathBuf)>,
    /// Refuse a plaintext connection even when the URL does not say `tls://`.
    pub required: bool,
}

impl NatsTls {
    /// Whether the connection refuses plaintext: when required, and whenever a
    /// CA or a client certificate is configured.
    ///
    /// Either one only means something over TLS. Without this, the client
    /// upgrades only when the server's first `INFO` asks for it, so a broker
    /// that allows plaintext — or anyone on the path who rewrites that `INFO` —
    /// would get the credentials in the clear and the pinned CA never checked.
    #[must_use]
    pub fn enforced(&self) -> bool {
        self.required || self.ca_file.is_some() || self.client_cert.is_some()
    }
}

/// Where `JetStream` keeps a stream's messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NatsStorage {
    /// On disk: survives a broker restart. The default.
    #[default]
    File,
    /// In memory: lost when the broker restarts.
    Memory,
}

impl NatsStorage {
    /// The `NOTEDTHAT_NATS_STORAGE` value that selects this.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Memory => "memory",
        }
    }
}

impl std::fmt::Display for NatsStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Settings applied to every stream `NotedThat` owns.
///
/// Each is `None` unless configured. A stream `NotedThat` creates then gets the
/// default; an existing stream keeps what it has, so a stream an operator scaled
/// or tuned by hand is not reset by an upgrade that introduced the setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NatsStreamSettings {
    /// How many broker nodes hold a copy; [`DEFAULT_NATS_REPLICAS`] on create.
    pub replicas: Option<usize>,
    /// Where the messages live; [`NatsStorage::File`] on create.
    pub storage: Option<NatsStorage>,
    /// How long the broker remembers a `Nats-Msg-Id` to drop a repeated publish;
    /// [`DEFAULT_NATS_DUPLICATE_WINDOW_SECS`], at most the retention, on create.
    pub duplicate_window: Option<Duration>,
}

/// The validated connection configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct NatsConnectConfig {
    /// Server URL, `nats://[user:pass@]host:port` or `tls://…`. Credentials may
    /// travel in the URL, which is why the server hides this value from `--help`
    /// and this type's `Debug` does not print it.
    pub url: String,
    /// Authentication beyond the URL.
    pub auth: NatsAuth,
    /// TLS towards the broker.
    pub tls: NatsTls,
    /// Settings applied to every NotedThat-owned stream.
    pub streams: NatsStreamSettings,
}

impl std::fmt::Debug for NatsConnectConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsConnectConfig")
            .field("url", &"<redacted>")
            .field("auth", &self.auth)
            .field("tls", &self.tls)
            .field("streams", &self.streams)
            .finish()
    }
}

impl NatsConnectConfig {
    /// A plain connection to `url` with every other setting at its default —
    /// what a test against a local broker needs.
    #[must_use]
    pub fn plain(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            auth: NatsAuth::None,
            tls: NatsTls::default(),
            streams: NatsStreamSettings::default(),
        }
    }

    /// Validate already-collected settings, whatever supplied them.
    ///
    /// # Errors
    ///
    /// `Error::Config` when the URL is absent or empty, more than one
    /// authentication method is supplied, a path is empty, only half of a
    /// client certificate pair is supplied, a number or choice is invalid, or
    /// the duplicate window is shorter than [`MIN_NATS_DUPLICATE_WINDOW_SECS`].
    pub fn from_settings(settings: NatsConnectSettings) -> Result<Self, Error> {
        let url = settings.url.filter(|url| !url.is_empty()).ok_or_else(|| {
            config_error(format!("{} is required", setting("NOTEDTHAT_NATS_URL")))
        })?;
        let auth = parse_auth(
            &url,
            settings.creds_file,
            settings.nkey_seed_file,
            settings.token,
        )?;

        let ca_file = parse_path("NOTEDTHAT_NATS_TLS_CA_FILE", settings.tls_ca_file)?;
        let cert = parse_path("NOTEDTHAT_NATS_TLS_CERT_FILE", settings.tls_cert_file)?;
        let key = parse_path("NOTEDTHAT_NATS_TLS_KEY_FILE", settings.tls_key_file)?;
        let client_cert = match (cert, key) {
            (Some(cert), Some(key)) => Some((cert, key)),
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                return Err(config_error(format!(
                    "{} and {} must be set together",
                    setting("NOTEDTHAT_NATS_TLS_CERT_FILE"),
                    setting("NOTEDTHAT_NATS_TLS_KEY_FILE")
                )));
            }
        };
        let required = parse_bool(
            "NOTEDTHAT_NATS_TLS_REQUIRED",
            settings.tls_required.as_deref(),
            false,
        )?;

        let replicas = parse_optional_positive(
            "NOTEDTHAT_NATS_REPLICAS",
            settings.replicas.as_deref(),
            "a number of replicas from 1 to 5",
        )?;
        if let Some(replicas) = replicas
            && replicas > MAX_NATS_REPLICAS
        {
            return Err(config_error(format!(
                "{} is invalid: expected a number of replicas from 1 to 5, got \"{replicas}\"",
                setting("NOTEDTHAT_NATS_REPLICAS")
            )));
        }
        let storage = match utf8("NOTEDTHAT_NATS_STORAGE", settings.storage.as_deref())? {
            None => None,
            Some("file") => Some(NatsStorage::File),
            Some("memory") => Some(NatsStorage::Memory),
            Some(other) => {
                return Err(config_error(format!(
                    "{} is invalid: expected \"file\" or \"memory\", got \"{other}\"",
                    setting("NOTEDTHAT_NATS_STORAGE")
                )));
            }
        };
        let duplicate_window = parse_optional_positive(
            "NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS",
            settings.duplicate_window_secs.as_deref(),
            "a positive number of seconds",
        )?;
        if let Some(window) = duplicate_window
            && window < MIN_NATS_DUPLICATE_WINDOW_SECS
        {
            return Err(config_error(format!(
                "{} is invalid: expected at least {MIN_NATS_DUPLICATE_WINDOW_SECS} seconds, got \
                 \"{window}\"; a publish retried after its acknowledgement timed out must still \
                 find the first copy inside the window",
                setting("NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS")
            )));
        }

        Ok(Self {
            url,
            auth,
            tls: NatsTls {
                ca_file,
                client_cert,
                required,
            },
            streams: NatsStreamSettings {
                // At most 5, checked above.
                replicas: replicas
                    .map(|replicas| usize::try_from(replicas).unwrap_or(DEFAULT_NATS_REPLICAS)),
                storage,
                duplicate_window: duplicate_window.map(Duration::from_secs),
            },
        })
    }
}

/// The one authentication method, counting credentials in the URL as one.
fn parse_auth(
    url: &str,
    creds_file: Option<OsString>,
    nkey_seed_file: Option<OsString>,
    token: Option<String>,
) -> Result<NatsAuth, Error> {
    let creds = parse_path("NOTEDTHAT_NATS_CREDS_FILE", creds_file)?;
    let nkey = parse_path("NOTEDTHAT_NATS_NKEY_SEED_FILE", nkey_seed_file)?;
    let token = match token {
        Some(token) if token.is_empty() => {
            return Err(config_error(format!(
                "{} must not be empty",
                setting("NOTEDTHAT_NATS_TOKEN")
            )));
        }
        other => other,
    };
    let supplied: Vec<String> = [
        split_url_credentials(url)
            .is_some()
            .then(|| format!("credentials in {}", setting("NOTEDTHAT_NATS_URL"))),
        creds
            .is_some()
            .then(|| setting("NOTEDTHAT_NATS_CREDS_FILE")),
        nkey.is_some()
            .then(|| setting("NOTEDTHAT_NATS_NKEY_SEED_FILE")),
        token.is_some().then(|| setting("NOTEDTHAT_NATS_TOKEN")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if supplied.len() > 1 {
        return Err(config_error(format!(
            "only one NATS authentication method may be set, got {}",
            supplied.join(" and ")
        )));
    }
    Ok(match (creds, nkey, token) {
        (Some(path), _, _) => NatsAuth::CredsFile(path),
        (_, Some(path), _) => NatsAuth::NkeySeedFile(path),
        (_, _, Some(token)) => NatsAuth::Token(token),
        _ => NatsAuth::None,
    })
}

/// Read a setting as UTF-8, `None` when absent. Empty is an error.
fn utf8<'a>(var: &str, supplied: Option<&'a OsStr>) -> Result<Option<&'a str>, Error> {
    let Some(value) = supplied else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| config_error(format!("{} must be valid UTF-8", setting(var))))?;
    if value.is_empty() {
        return Err(config_error(format!("{} must not be empty", setting(var))));
    }
    Ok(Some(value))
}

fn parse_path(var: &str, supplied: Option<OsString>) -> Result<Option<PathBuf>, Error> {
    match supplied {
        None => Ok(None),
        Some(path) if path.is_empty() => {
            Err(config_error(format!("{} must not be empty", setting(var))))
        }
        Some(path) => Ok(Some(PathBuf::from(path))),
    }
}

fn parse_bool(var: &str, supplied: Option<&OsStr>, default: bool) -> Result<bool, Error> {
    match utf8(var, supplied)? {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(config_error(format!(
            "{} is invalid: expected \"true\" or \"false\", got \"{other}\"",
            setting(var)
        ))),
    }
}

/// Parse a positive integer setting, `default` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting when the value is not UTF-8, empty,
/// not a number, or zero.
pub fn parse_positive(
    var: &str,
    supplied: Option<&OsStr>,
    default: u64,
    accepted: &str,
) -> Result<u64, Error> {
    let Some(value) = utf8(var, supplied)? else {
        return Ok(default);
    };
    value.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
        config_error(format!(
            "{} is invalid: expected {accepted}, got \"{value}\"",
            setting(var)
        ))
    })
}

/// Credentials carried in a NATS URL's userinfo.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum UrlCredentials {
    /// `nats://user:pass@host`.
    UserPassword(String, String),
    /// `nats://token@host`, which is how the other NATS clients read a user
    /// without a password.
    Token(String),
}

/// Split the credentials out of a NATS URL, percent-decoded, and return the URL
/// without them.
///
/// async-nats parses the userinfo of a URL but never sends it, so a URL that
/// carries credentials has to be turned into connect options. `None` when the
/// URL carries none, or does not parse (connecting reports that).
pub(crate) fn split_url_credentials(url: &str) -> Option<(UrlCredentials, String)> {
    // A URL without a scheme is a `nats://` one, as async-nats reads it.
    let mut parsed = if url.contains("://") {
        url::Url::parse(url)
    } else {
        url::Url::parse(&format!("nats://{url}"))
    }
    .ok()?;
    if parsed.username().is_empty() {
        return None;
    }
    let decode = |part: &str| {
        percent_encoding::percent_decode_str(part)
            .decode_utf8_lossy()
            .into_owned()
    };
    let user = decode(parsed.username());
    let credentials = match parsed.password() {
        Some(password) => UrlCredentials::UserPassword(user, decode(password)),
        None => UrlCredentials::Token(user),
    };
    parsed.set_username("").ok()?;
    parsed.set_password(None).ok()?;
    Some((credentials, parsed.into()))
}

/// Parse a positive integer setting, `None` when absent.
fn parse_optional_positive(
    var: &str,
    supplied: Option<&OsStr>,
    accepted: &str,
) -> Result<Option<u64>, Error> {
    supplied
        .map(|value| parse_positive(var, Some(value), 0, accepted))
        .transpose()
}

/// Validate a `JetStream` stream (or consumer) name setting, `default` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting when the name is empty or has a
/// character `JetStream` refuses.
pub fn parse_stream_name(
    var: &str,
    supplied: Option<String>,
    default: &str,
) -> Result<String, Error> {
    match supplied {
        None => Ok(default.to_string()),
        Some(name) if name.is_empty() => {
            Err(config_error(format!("{} must not be empty", setting(var))))
        }
        Some(name) => {
            if name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                Ok(name)
            } else {
                Err(config_error(format!(
                    "{} is invalid: expected letters, digits, '_' or '-', got \"{name}\"",
                    setting(var)
                )))
            }
        }
    }
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

    fn with_url() -> NatsConnectSettings {
        NatsConnectSettings {
            url: Some("nats://localhost:4222".into()),
            ..NatsConnectSettings::default()
        }
    }

    #[test]
    fn a_url_is_required_and_the_rest_defaults() {
        let err =
            message(NatsConnectConfig::from_settings(NatsConnectSettings::default()).unwrap_err());
        assert_eq!(err, "NOTEDTHAT_NATS_URL (--nats-url) is required");

        let config = NatsConnectConfig::from_settings(with_url()).unwrap();
        assert_eq!(config, NatsConnectConfig::plain("nats://localhost:4222"));
        // Unset stream settings stay unset: an existing stream keeps its own.
        assert_eq!(config.streams, NatsStreamSettings::default());
        assert_eq!(config.streams.replicas, None);
    }

    #[test]
    fn credentials_in_the_url_are_split_out_and_decoded() {
        let (credentials, url) =
            split_url_credentials("nats://alice:s%40cr%3At@broker:4222").unwrap();
        assert!(
            credentials == UrlCredentials::UserPassword("alice".into(), "s@cr:t".into()),
            "user and password, percent-decoded"
        );
        assert_eq!(url, "nats://broker:4222");

        let (credentials, url) = split_url_credentials("tls://tok3n@broker:4222").unwrap();
        assert!(credentials == UrlCredentials::Token("tok3n".into()));
        assert_eq!(url, "tls://broker:4222");

        // No scheme is a `nats://` URL, as async-nats reads it.
        let (_, url) = split_url_credentials("alice:pw@broker:4222").unwrap();
        assert_eq!(url, "nats://broker:4222");

        assert!(split_url_credentials("nats://broker:4222").is_none());
        assert!(split_url_credentials("broker:4222").is_none());
    }

    #[test]
    fn credentials_in_the_url_count_as_an_authentication_method() {
        let alone = NatsConnectConfig::from_settings(NatsConnectSettings {
            url: Some("nats://alice:pw@broker:4222".into()),
            ..NatsConnectSettings::default()
        })
        .unwrap();
        assert_eq!(alone.auth, NatsAuth::None);

        let err = message(
            NatsConnectConfig::from_settings(NatsConnectSettings {
                url: Some("nats://alice:pw@broker:4222".into()),
                token: Some("t".into()),
                ..NatsConnectSettings::default()
            })
            .unwrap_err(),
        );
        assert!(err.contains("only one NATS authentication method"), "{err}");
        assert!(
            err.contains("credentials in NOTEDTHAT_NATS_URL (--nats-url)"),
            "{err}"
        );
        assert!(err.contains("NOTEDTHAT_NATS_TOKEN (--nats-token)"), "{err}");
        assert!(
            !err.contains("pw"),
            "the error must not quote the password: {err}"
        );
    }

    #[test]
    fn each_authentication_method_is_accepted_alone() {
        let creds = NatsConnectConfig::from_settings(NatsConnectSettings {
            creds_file: Some("/run/secrets/nt.creds".into()),
            ..with_url()
        })
        .unwrap();
        assert_eq!(
            creds.auth,
            NatsAuth::CredsFile("/run/secrets/nt.creds".into())
        );

        let nkey = NatsConnectConfig::from_settings(NatsConnectSettings {
            nkey_seed_file: Some("/run/secrets/nt.nk".into()),
            ..with_url()
        })
        .unwrap();
        assert_eq!(
            nkey.auth,
            NatsAuth::NkeySeedFile("/run/secrets/nt.nk".into())
        );

        let token = NatsConnectConfig::from_settings(NatsConnectSettings {
            token: Some("s3cret".into()),
            ..with_url()
        })
        .unwrap();
        assert_eq!(token.auth, NatsAuth::Token("s3cret".into()));
    }

    #[test]
    fn two_authentication_methods_are_refused_by_name() {
        let err = message(
            NatsConnectConfig::from_settings(NatsConnectSettings {
                creds_file: Some("/a.creds".into()),
                token: Some("t".into()),
                ..with_url()
            })
            .unwrap_err(),
        );
        assert!(err.contains("only one NATS authentication method"), "{err}");
        assert!(
            err.contains("NOTEDTHAT_NATS_CREDS_FILE (--nats-creds-file)"),
            "{err}"
        );
        assert!(err.contains("NOTEDTHAT_NATS_TOKEN (--nats-token)"), "{err}");
    }

    #[test]
    fn empty_values_are_refused_rather_than_ignored() {
        for settings in [
            NatsConnectSettings {
                token: Some(String::new()),
                ..with_url()
            },
            NatsConnectSettings {
                creds_file: Some(OsString::new()),
                ..with_url()
            },
            NatsConnectSettings {
                tls_ca_file: Some(OsString::new()),
                ..with_url()
            },
            NatsConnectSettings {
                storage: Some(OsString::new()),
                ..with_url()
            },
        ] {
            let err = message(NatsConnectConfig::from_settings(settings).unwrap_err());
            assert!(err.ends_with("must not be empty"), "{err}");
        }
    }

    #[test]
    fn a_client_certificate_needs_its_key() {
        let ok = NatsConnectConfig::from_settings(NatsConnectSettings {
            tls_cert_file: Some("/c.pem".into()),
            tls_key_file: Some("/k.pem".into()),
            tls_ca_file: Some("/ca.pem".into()),
            tls_required: Some("true".into()),
            ..with_url()
        })
        .unwrap();
        assert_eq!(
            ok.tls,
            NatsTls {
                ca_file: Some("/ca.pem".into()),
                client_cert: Some(("/c.pem".into(), "/k.pem".into())),
                required: true,
            }
        );

        for settings in [
            NatsConnectSettings {
                tls_cert_file: Some("/c.pem".into()),
                ..with_url()
            },
            NatsConnectSettings {
                tls_key_file: Some("/k.pem".into()),
                ..with_url()
            },
        ] {
            let err = message(NatsConnectConfig::from_settings(settings).unwrap_err());
            assert!(err.contains("must be set together"), "{err}");
        }
    }

    #[test]
    fn a_ca_or_client_certificate_makes_tls_required() {
        assert!(!NatsTls::default().enforced());
        for settings in [
            NatsConnectSettings {
                tls_ca_file: Some("/ca.pem".into()),
                ..with_url()
            },
            NatsConnectSettings {
                tls_cert_file: Some("/c.pem".into()),
                tls_key_file: Some("/k.pem".into()),
                ..with_url()
            },
            NatsConnectSettings {
                tls_required: Some("true".into()),
                ..with_url()
            },
        ] {
            let config = NatsConnectConfig::from_settings(settings).unwrap();
            assert!(config.tls.enforced(), "{:?}", config.tls);
        }
    }

    #[test]
    fn tls_required_is_strictly_true_or_false() {
        let err = message(
            NatsConnectConfig::from_settings(NatsConnectSettings {
                tls_required: Some("yes".into()),
                ..with_url()
            })
            .unwrap_err(),
        );
        assert!(
            err.contains("NOTEDTHAT_NATS_TLS_REQUIRED (--nats-tls-required)"),
            "{err}"
        );
    }

    #[test]
    fn stream_settings_are_validated() {
        let ok = NatsConnectConfig::from_settings(NatsConnectSettings {
            replicas: Some("3".into()),
            storage: Some("memory".into()),
            duplicate_window_secs: Some("30".into()),
            ..with_url()
        })
        .unwrap();
        assert_eq!(
            ok.streams,
            NatsStreamSettings {
                replicas: Some(3),
                storage: Some(NatsStorage::Memory),
                duplicate_window: Some(Duration::from_secs(30)),
            }
        );

        for (settings, var) in [
            (
                NatsConnectSettings {
                    replicas: Some("0".into()),
                    ..with_url()
                },
                "NOTEDTHAT_NATS_REPLICAS",
            ),
            (
                NatsConnectSettings {
                    replicas: Some("6".into()),
                    ..with_url()
                },
                "NOTEDTHAT_NATS_REPLICAS",
            ),
            (
                NatsConnectSettings {
                    storage: Some("disk".into()),
                    ..with_url()
                },
                "NOTEDTHAT_NATS_STORAGE",
            ),
            (
                NatsConnectSettings {
                    duplicate_window_secs: Some("0".into()),
                    ..with_url()
                },
                "NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS",
            ),
        ] {
            let err = message(NatsConnectConfig::from_settings(settings).unwrap_err());
            assert!(err.contains(var), "{err}");
        }
    }

    #[test]
    fn a_duplicate_window_must_outlast_a_timed_out_acknowledgement() {
        let window = |secs: &str| {
            NatsConnectConfig::from_settings(NatsConnectSettings {
                duplicate_window_secs: Some(secs.into()),
                ..with_url()
            })
        };
        assert_eq!(MIN_NATS_DUPLICATE_WINDOW_SECS, 2 * crate::TIMEOUT.as_secs());
        let min = MIN_NATS_DUPLICATE_WINDOW_SECS.to_string();
        assert_eq!(
            window(&min).unwrap().streams.duplicate_window,
            Some(Duration::from_secs(MIN_NATS_DUPLICATE_WINDOW_SECS))
        );
        for short in ["1", "5", &(MIN_NATS_DUPLICATE_WINDOW_SECS - 1).to_string()] {
            let err = message(window(short).unwrap_err());
            assert!(
                err.contains("NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS (--nats-duplicate-window-secs)"),
                "{err}"
            );
            assert!(err.contains(&format!("at least {min} seconds")), "{err}");
        }
    }

    #[test]
    fn debug_never_prints_a_credential() {
        // Two configs: credentials in the URL and a token are mutually exclusive.
        for settings in [
            NatsConnectSettings {
                url: Some("nats://u:url-leak-canary@broker:4222".into()),
                ..NatsConnectSettings::default()
            },
            NatsConnectSettings {
                token: Some("token-leak-canary".into()),
                ..with_url()
            },
        ] {
            let config = NatsConnectConfig::from_settings(settings).unwrap();
            let printed = format!("{config:?}");
            assert!(!printed.contains("leak-canary"), "{printed}");
        }
    }

    #[test]
    fn from_env_reads_exactly_the_inventoried_variables() {
        let vars: Vec<(&str, Option<&str>)> = vec![
            ("NOTEDTHAT_NATS_URL", Some("tls://broker:4222")),
            ("NOTEDTHAT_NATS_CREDS_FILE", None),
            ("NOTEDTHAT_NATS_NKEY_SEED_FILE", Some("/seed")),
            ("NOTEDTHAT_NATS_TOKEN", None),
            ("NOTEDTHAT_NATS_TLS_CA_FILE", Some("/ca.pem")),
            ("NOTEDTHAT_NATS_TLS_CERT_FILE", Some("/c.pem")),
            ("NOTEDTHAT_NATS_TLS_KEY_FILE", Some("/k.pem")),
            ("NOTEDTHAT_NATS_TLS_REQUIRED", Some("true")),
            ("NOTEDTHAT_NATS_REPLICAS", Some("3")),
            ("NOTEDTHAT_NATS_STORAGE", Some("file")),
            ("NOTEDTHAT_NATS_DUPLICATE_WINDOW_SECS", Some("60")),
        ];
        assert_eq!(
            vars.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            NATS_CONNECT_ENV_VARS.to_vec()
        );
        temp_env::with_vars(vars, || {
            let config = NatsConnectConfig::from_settings(NatsConnectSettings::from_env()).unwrap();
            assert_eq!(config.url, "tls://broker:4222");
            assert_eq!(config.auth, NatsAuth::NkeySeedFile("/seed".into()));
            assert!(config.tls.required);
            assert_eq!(config.streams.replicas, Some(3));
            assert_eq!(
                config.streams.duplicate_window,
                Some(Duration::from_secs(60))
            );
        });
    }
}
