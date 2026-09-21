//! Configuration for `notedthat-server`.
//!
//! Every setting arrives from one of two places: the command line, or the
//! process environment. [`crate::cli::ServerCli`] resolves which — the flag wins
//! — and hands the raw values here; nothing in this module reads the
//! environment itself. See `docs/CONFIGURATION.md` for the full reference.

use crate::cli::ServerCli;
use crate::oidc::OidcSettings;
use notedthat_core::{Error, KbSlug, StagingConfig, TenantSlug, setting};
use notedthat_events::{MemoryConfig, MemorySettings, NatsConfig, NatsSettings};
use notedthat_storage_fs::FsSettings;
use notedthat_storage_s3::S3Settings;
use notedthat_write::MAX_UPLOAD_BYTES;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::time::Duration;

/// Which storage backend the server runs on.
///
/// Parsed separately from its configuration so the selection can be named in an error
/// message before any backend configuration is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StorageBackendKind {
    /// An S3-compatible object store.
    S3,
    /// A local filesystem tree.
    Fs,
}

impl StorageBackendKind {
    /// The `NOTEDTHAT_STORAGE_BACKEND` value that selects this backend.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Fs => "fs",
        }
    }
}

impl std::fmt::Display for StorageBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The selected storage backend together with the configuration it needs.
///
/// An enum rather than one `Option` per backend, so "exactly one backend is configured"
/// is a property of the type and [`crate::run`] has no unreachable error arm.
#[derive(Debug, Clone)]
pub enum StorageConfig {
    /// S3-compatible object store (the default).
    S3(notedthat_storage_s3::S3Config),
    /// Local filesystem tree.
    Fs(notedthat_storage_fs::FsConfig),
}

impl StorageConfig {
    /// Which backend this is.
    #[must_use]
    pub fn kind(&self) -> StorageBackendKind {
        match self {
            Self::S3(_) => StorageBackendKind::S3,
            Self::Fs(_) => StorageBackendKind::Fs,
        }
    }
}

/// Which object change event log the server publishes to (`NOTEDTHAT_EVENTS_BACKEND`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventsBackendKind {
    /// No log: writes are not announced and the events route answers 404.
    None,
    /// A process-local ring — replay across reconnects, not restarts or replicas.
    Memory,
    /// A NATS `JetStream` stream shared by every replica.
    Nats,
}

impl EventsBackendKind {
    /// The `NOTEDTHAT_EVENTS_BACKEND` value that selects this backend.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Memory => "memory",
            Self::Nats => "nats",
        }
    }
}

impl std::fmt::Display for EventsBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether `/mcp` admits a request that presents no credential (`NOTEDTHAT_MCP_ANONYMOUS`).
///
/// The MCP surface acts as its caller on the loopback API, so an anonymous caller is bound
/// by the manifests' `anyone` rules exactly as a direct anonymous request is. What this
/// setting decides is only whether such a request is let in at all, because the alternative
/// — a `401` with the bearer challenge — is what an OAuth-capable MCP client needs to see
/// before it will sign in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpAnonymous {
    /// Admit anonymous callers when at least one declared knowledge base grants `anyone`
    /// something; otherwise `401`. The default.
    #[default]
    Auto,
    /// Always `401` a missing credential, whatever the manifests grant — for a deployment
    /// with public knowledge bases and an identity provider whose operator wants OAuth
    /// clients challenged on connect rather than signed in by hand.
    Never,
}

impl McpAnonymous {
    /// The `NOTEDTHAT_MCP_ANONYMOUS` value that selects this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Never => "never",
        }
    }
}

impl std::fmt::Display for McpAnonymous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The selected events backend together with the configuration it needs.
#[derive(Debug, Clone)]
pub enum EventsConfig {
    /// No event log (the default).
    None,
    /// The in-process ring.
    Memory(MemoryConfig),
    /// A NATS `JetStream` stream.
    Nats(NatsConfig),
}

impl EventsConfig {
    /// Which backend this is.
    #[must_use]
    pub fn kind(&self) -> EventsBackendKind {
        match self {
            Self::None => EventsBackendKind::None,
            Self::Memory(_) => EventsBackendKind::Memory,
            Self::Nats(_) => EventsBackendKind::Nats,
        }
    }
}

/// Every setting owned by one storage backend, paired with its backend and with
/// whether this run supplied it at all.
///
/// A setting whose owner is not the selected backend is a startup error rather than an
/// ignored setting — silently ignoring `NOTEDTHAT_FS_ROOT` under the default `s3` backend
/// is how an operator ends up believing their bytes are on a disk they are not on. Same
/// reasoning as [`REMOVED_LISTENER_ENV_VARS`] (D39), applied to backend selection.
///
/// "Supplied" means presence, not value, and spans both sources: `--s3-region ""` and
/// `NOTEDTHAT_S3_REGION=` both count, matching how an empty variable counted when the
/// environment was the only source.
///
/// Deliberately confined to settings this server reads. `AWS_*` is not listed:
/// `S3Config::build_client` uses a static credential provider and never consults the
/// ambient credential chain, so rejecting an `AWS_ACCESS_KEY_ID` on a shared runner would
/// be a pure false positive.
///
/// The names are asserted against each adapter's own inventory — `S3_ENV_VARS` and
/// `FS_ENV_VARS` — by a test, so this table cannot drift from what those adapters read.
fn backend_owned_settings(cli: &ServerCli) -> Vec<(&'static str, StorageBackendKind, bool)> {
    use StorageBackendKind::{Fs, S3};
    vec![
        ("NOTEDTHAT_S3_REGION", S3, cli.s3_region.is_some()),
        (
            "NOTEDTHAT_S3_ACCESS_KEY_ID",
            S3,
            cli.s3_access_key_id.is_some(),
        ),
        (
            "NOTEDTHAT_S3_SECRET_ACCESS_KEY",
            S3,
            cli.s3_secret_access_key.is_some(),
        ),
        (
            "NOTEDTHAT_S3_ENDPOINT_URL",
            S3,
            cli.s3_endpoint_url.is_some(),
        ),
        (
            "NOTEDTHAT_S3_FORCE_PATH_STYLE",
            S3,
            cli.s3_force_path_style.is_some(),
        ),
        ("NOTEDTHAT_FS_ROOT", Fs, cli.fs_root.is_some()),
        ("NOTEDTHAT_FS_METADATA", Fs, cli.fs_metadata.is_some()),
        ("NOTEDTHAT_FS_FILE_MODE", Fs, cli.fs_file_mode.is_some()),
        ("NOTEDTHAT_FS_DIR_MODE", Fs, cli.fs_dir_mode.is_some()),
        (
            "NOTEDTHAT_FS_ALLOW_LOSSY_NAMES",
            Fs,
            cli.fs_allow_lossy_names.is_some(),
        ),
        ("NOTEDTHAT_FS_WATCH", Fs, cli.fs_watch.is_some()),
        (
            "NOTEDTHAT_FS_WATCH_DEBOUNCE_MS",
            Fs,
            cli.fs_watch_debounce_ms.is_some(),
        ),
    ]
}

/// The `NOTEDTHAT_EVENTS_*` and `NOTEDTHAT_NATS_*` settings, each with the events
/// backend that reads it. Same purpose and same guard as [`backend_owned_settings`].
fn events_owned_settings(cli: &ServerCli) -> Vec<(&'static str, EventsBackendKind, bool)> {
    use EventsBackendKind::{Memory, Nats};
    vec![
        (
            "NOTEDTHAT_EVENTS_MEMORY_CAPACITY",
            Memory,
            cli.events_memory_capacity.is_some(),
        ),
        ("NOTEDTHAT_NATS_URL", Nats, cli.nats_url.is_some()),
        ("NOTEDTHAT_NATS_STREAM", Nats, cli.nats_stream.is_some()),
        (
            "NOTEDTHAT_NATS_MAX_AGE_SECS",
            Nats,
            cli.nats_max_age_secs.is_some(),
        ),
    ]
}

/// Parse the backend selector, returning `None` when it was not supplied.
///
/// Strict, unlike `NOTEDTHAT_LOG_FORMAT` and `NOTEDTHAT_S3_FORCE_PATH_STYLE`, which
/// silently fall back on an unrecognised value. Those two can afford leniency because a
/// mis-parse announces itself immediately — the wrong log format is visible in the first
/// line of output, and a wrong path-style setting fails on the first request. A backend
/// selector cannot: `NOTEDTHAT_STORAGE_BACKEND=fs3` would fall back to `s3`, start
/// cleanly, provision buckets and serve a knowledge base that looks empty because the
/// operator's data is on disk. Nothing later in the run would say so.
fn parse_storage_backend(supplied: Option<&OsStr>) -> Result<Option<StorageBackendKind>, Error> {
    let Some(value) = supplied else {
        return Ok(None);
    };
    let name = setting("NOTEDTHAT_STORAGE_BACKEND");
    let value = value.to_str().ok_or_else(|| Error::Config {
        message: format!("{name} must be valid UTF-8"),
    })?;
    if value.is_empty() {
        return Err(Error::Config {
            message: format!("{name} must not be empty"),
        });
    }
    match value {
        "s3" => Ok(Some(StorageBackendKind::S3)),
        "fs" => Ok(Some(StorageBackendKind::Fs)),
        other => Err(Error::Config {
            message: format!("{name} is invalid: expected \"s3\" or \"fs\", got \"{other}\""),
        }),
    }
}

/// Parse the events backend selector, returning `None` when it was not supplied.
///
/// Strict for the same reason as [`parse_storage_backend`]: a mis-spelled selector
/// that fell back to `none` would start cleanly and simply never announce anything.
fn parse_events_backend(supplied: Option<&OsStr>) -> Result<Option<EventsBackendKind>, Error> {
    let Some(value) = supplied else {
        return Ok(None);
    };
    let name = setting("NOTEDTHAT_EVENTS_BACKEND");
    let value = value.to_str().ok_or_else(|| Error::Config {
        message: format!("{name} must be valid UTF-8"),
    })?;
    if value.is_empty() {
        return Err(Error::Config {
            message: format!("{name} must not be empty"),
        });
    }
    match value {
        "none" => Ok(Some(EventsBackendKind::None)),
        "memory" => Ok(Some(EventsBackendKind::Memory)),
        "nats" => Ok(Some(EventsBackendKind::Nats)),
        other => Err(Error::Config {
            message: format!(
                "{name} is invalid: expected \"none\", \"memory\" or \"nats\", got \"{other}\""
            ),
        }),
    }
}

/// Parse `NOTEDTHAT_MCP_ANONYMOUS`.
///
/// An empty or blank value is the default, as it is for the sibling
/// `NOTEDTHAT_MCP_HTTP_*` settings: Compose passes every MCP variable through as `${VAR-}`,
/// so an operator who never set it hands the server an empty string. Anything else that is
/// not a mode is refused, as the backend selectors are — the wrong spelling of `never` must
/// not quietly become `auto`.
fn parse_mcp_anonymous(supplied: Option<&str>) -> Result<McpAnonymous, Error> {
    let value = supplied.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(McpAnonymous::default());
    }
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(McpAnonymous::Auto),
        "never" => Ok(McpAnonymous::Never),
        other => Err(Error::Config {
            message: format!(
                "{} is invalid: expected \"auto\" or \"never\", got \"{other}\"",
                setting("NOTEDTHAT_MCP_ANONYMOUS")
            ),
        }),
    }
}

/// Refuse to start when settings belonging to an unselected backend are supplied.
///
/// Reports every offender at once: the realistic case is a whole `NOTEDTHAT_S3_*` family
/// left behind by an operator switching to `fs`, and naming one per restart would take
/// five restarts.
///
/// The check runs when the selector is unset too, and says so. That is the highest-value
/// case: an operator who sets `NOTEDTHAT_FS_ROOT` and forgets the selector would
/// otherwise get a perfectly healthy S3 deployment with an unread root.
///
/// Generic over the selector so the storage and events backends share one message
/// shape. Offenders are grouped by the backend that owns them, since with three
/// events backends the unselected ones are not a single "other".
fn reject_unselected_settings<K: Copy + Eq + Ord + std::fmt::Display>(
    selector: &'static str,
    selected: Option<K>,
    default: K,
    table: Vec<(&'static str, K, bool)>,
) -> Result<(), Error> {
    let effective = selected.unwrap_or(default);
    let mut by_owner: BTreeMap<K, Vec<String>> = BTreeMap::new();
    for (name, owner, supplied) in table {
        if supplied && owner != effective {
            by_owner.entry(owner).or_default().push(setting(name));
        }
    }

    if by_owner.is_empty() {
        return Ok(());
    }

    let selection = match selected {
        Some(kind) => format!("{} is {kind}", setting(selector)),
        None => format!(
            "{} is unset, so the default {default} backend is selected",
            setting(selector)
        ),
    };
    let complaints: Vec<String> = by_owner
        .iter()
        .map(|(owner, names)| {
            format!(
                "these settings belong to the {owner} backend and would be ignored: {}",
                names.join(", ")
            )
        })
        .collect();
    let fixes: Vec<String> = by_owner
        .keys()
        .map(|owner| format!("{selector}={owner}"))
        .collect();
    Err(Error::Config {
        message: format!(
            "{selection}, but {}. Unset them or set {} to start the server.",
            complaints.join("; "),
            fixes.join(" or ")
        ),
    })
}

/// An S3 storage config pointed at an unroutable address.
///
/// For tests that inject their own [`crate::run::Backends`] and never build a client
/// from it. A regression that *does* reach for it fails loudly rather than quietly
/// talking to something real.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn unroutable_storage_placeholder() -> StorageConfig {
    StorageConfig::S3(notedthat_storage_s3::S3Config {
        endpoint_url: Some("http://127.0.0.1:1".to_string()),
        region: "us-east-1".to_string(),
        access_key_id: "any".to_string(),
        secret_access_key: "any".to_string(),
        force_path_style: true,
    })
}

/// Server-wide configuration.
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
    /// The selected storage backend and its configuration
    /// (`NOTEDTHAT_STORAGE_BACKEND`; default `s3`).
    pub storage: StorageConfig,
    /// The selected object change event log and its configuration
    /// (`NOTEDTHAT_EVENTS_BACKEND`; default `none`).
    pub events: EventsConfig,
    /// Log output format (`NOTEDTHAT_LOG_FORMAT`; `pretty` or `json`).
    pub log_format: LogFormat,
    /// Qdrant client configuration.
    pub qdrant: ServerQdrantConfig,
    /// Embedder configuration.
    pub embedder: EmbedderConfig,
    /// `WebDAV` Basic authentication username (`NOTEDTHAT_WEBDAV_USERNAME`; required).
    pub webdav_username: String,
    /// `WebDAV` Basic authentication password (`NOTEDTHAT_WEBDAV_PASSWORD`; required).
    pub webdav_password: String,
    /// Allowed origins for MCP HTTP CORS (`NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS`; empty → `["null"]`).
    pub mcp_http_allowed_origins: Vec<String>,
    /// Allowed hosts for MCP HTTP Host header validation (`NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS`; empty → `["127.0.0.1", "localhost", "::1"]`).
    pub mcp_http_allowed_hosts: Vec<String>,
    /// Whether `/mcp` admits anonymous callers (`NOTEDTHAT_MCP_ANONYMOUS`; default `auto`).
    pub mcp_anonymous: McpAnonymous,
    /// Maximum patchable object size in bytes (`NOTEDTHAT_MAX_PATCHABLE_SIZE`; default 100 MiB).
    pub max_patchable_size: u64,
    /// Most bytes one MCP object read may fetch (`NOTEDTHAT_MCP_MAX_READ_BYTES`; default 16 MiB,
    /// the API body cap). Larger objects are read in slices.
    pub mcp_max_read_bytes: u64,
    /// Shared private staging directory for uploads and index snapshots (`NOTEDTHAT_UPLOAD_TMP_DIR`).
    pub staging: StagingConfig,
    /// Identity-provider settings (`NOTEDTHAT_OIDC_*`); `None` when no issuer is set.
    pub oidc: Option<OidcSettings>,
}

/// Settings removed when the API, `WebDAV`, and MCP surfaces moved onto one
/// listener, each paired with the setup that replaces it.
///
/// Leaving one of these set is a silent exposure change on upgrade — a
/// `WebDAV` listener that was bound to loopback becomes reachable at `/webdav` on the
/// public listener, and `NOTEDTHAT_MCP_HTTP_ENABLED=false` no longer disables
/// `/mcp`. Per D39 the server refuses to start instead, naming the replacement.
///
/// [`ServerCli`] still accepts each one as a hidden flag for the same reason it is
/// checked here: an operator who reaches for the removed setting deserves the
/// replacement, not "unexpected argument".
const REMOVED_LISTENER_ENV_VARS: [(&str, &str); 3] = [
    (
        "NOTEDTHAT_WEBDAV_LISTEN_ADDR",
        "WebDAV is always served at /webdav on NOTEDTHAT_LISTEN_ADDR",
    ),
    (
        "NOTEDTHAT_MCP_HTTP_BIND",
        "MCP HTTP is always served at /mcp on NOTEDTHAT_LISTEN_ADDR",
    ),
    (
        "NOTEDTHAT_MCP_HTTP_ENABLED",
        "MCP HTTP is always served at /mcp on NOTEDTHAT_LISTEN_ADDR",
    ),
];

/// Tracing output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable multi-line output (default).
    Pretty,
    /// Machine-readable JSON (one line per event).
    Json,
}

impl Config {
    /// Parse configuration from the environment alone.
    ///
    /// Equivalent to [`Config::from_cli`] over an empty `argv`, which is what a
    /// container that passes no arguments gets.
    ///
    /// # Errors
    ///
    /// As [`Config::from_cli`], plus `Err(Error::Config { .. })` if a variable holds a
    /// value the parser cannot accept at all.
    pub fn from_env() -> Result<Self, Error> {
        let cli = ServerCli::from_env().map_err(|error| Error::Config {
            message: error.to_string(),
        })?;
        Self::from_cli(cli)
    }

    /// Validate the settings this run supplied, from either source.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` if any required setting is missing,
    /// if any value is invalid (empty token, bad slug, duplicate slug, etc.), or
    /// if any [`REMOVED_LISTENER_ENV_VARS`] entry was supplied.
    #[allow(clippy::too_many_lines)]
    pub fn from_cli(mut cli: ServerCli) -> Result<Self, Error> {
        let removed = [
            cli.webdav_listen_addr.is_some(),
            cli.mcp_http_bind.is_some(),
            cli.mcp_http_enabled.is_some(),
        ];
        for ((key, replacement), supplied) in REMOVED_LISTENER_ENV_VARS.iter().zip(removed) {
            if supplied {
                return Err(Error::Config {
                    message: format!(
                        "{key} was removed: {replacement}. Unset {key} to start the server."
                    ),
                });
            }
        }

        // Borrows the whole CLI, so it runs before the field-by-field moves below.
        let oidc = parse_oidc(&cli)?;

        let api_token = cli.api_token.take().ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_API_TOKEN")),
        })?;
        if api_token.trim().is_empty() {
            return Err(Error::Config {
                message: format!("{} must not be empty", setting("NOTEDTHAT_API_TOKEN")),
            });
        }

        let kbs_raw = cli.kbs.take().ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_KBS")),
        })?;
        if kbs_raw.trim().is_empty() {
            return Err(Error::Config {
                message: format!(
                    "{} must declare at least one knowledge base",
                    setting("NOTEDTHAT_KBS")
                ),
            });
        }

        let mut kbs = BTreeMap::new();
        for token in kbs_raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let slug = KbSlug::try_new(token).map_err(|e| Error::Config {
                message: format!("invalid KB slug {token:?}: {e}"),
            })?;
            if kbs.insert(slug.as_str().to_string(), slug).is_some() {
                return Err(Error::Config {
                    message: format!(
                        "duplicate KB slug in {}: {token:?}",
                        setting("NOTEDTHAT_KBS")
                    ),
                });
            }
        }
        if kbs.is_empty() {
            return Err(Error::Config {
                message: format!(
                    "{} must declare at least one knowledge base",
                    setting("NOTEDTHAT_KBS")
                ),
            });
        }

        // Tenant slug is hardcoded to "default" per Metis directive.
        // NOTEDTHAT_TENANT_SLUG intentionally not read.
        let tenant_slug = TenantSlug::default();

        // Taken, not moved: the backend-rejection check below needs the whole
        // `cli` by reference, and reordering the two would change which error an
        // operator sees when both are wrong.
        let listen_addr_str = cli
            .listen_addr
            .take()
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let listen_addr: SocketAddr = listen_addr_str.parse().map_err(|e| Error::Config {
            message: format!("{} is invalid: {e}", setting("NOTEDTHAT_LISTEN_ADDR")),
        })?;

        let selected = parse_storage_backend(cli.storage_backend.as_deref())?;
        reject_unselected_settings(
            "NOTEDTHAT_STORAGE_BACKEND",
            selected,
            StorageBackendKind::S3,
            backend_owned_settings(&cli),
        )?;
        let selected_events = parse_events_backend(cli.events_backend.as_deref())?;
        reject_unselected_settings(
            "NOTEDTHAT_EVENTS_BACKEND",
            selected_events,
            EventsBackendKind::None,
            events_owned_settings(&cli),
        )?;
        let storage = match selected.unwrap_or(StorageBackendKind::S3) {
            StorageBackendKind::S3 => {
                StorageConfig::S3(notedthat_storage_s3::S3Config::from_settings(S3Settings {
                    region: cli.s3_region,
                    access_key_id: cli.s3_access_key_id,
                    secret_access_key: cli.s3_secret_access_key,
                    endpoint_url: cli.s3_endpoint_url,
                    force_path_style: cli.s3_force_path_style,
                })?)
            }
            StorageBackendKind::Fs => {
                StorageConfig::Fs(notedthat_storage_fs::FsConfig::from_settings(FsSettings {
                    root: cli.fs_root,
                    metadata: cli.fs_metadata,
                    file_mode: cli.fs_file_mode,
                    dir_mode: cli.fs_dir_mode,
                    allow_lossy_names: cli.fs_allow_lossy_names,
                    watch: cli.fs_watch,
                    watch_debounce_ms: cli.fs_watch_debounce_ms,
                })?)
            }
        };

        let events = match selected_events.unwrap_or(EventsBackendKind::None) {
            EventsBackendKind::None => EventsConfig::None,
            EventsBackendKind::Memory => {
                EventsConfig::Memory(MemoryConfig::from_settings(&MemorySettings {
                    capacity: cli.events_memory_capacity,
                })?)
            }
            EventsBackendKind::Nats => {
                EventsConfig::Nats(NatsConfig::from_settings(NatsSettings {
                    url: cli.nats_url,
                    stream: cli.nats_stream,
                    max_age_secs: cli.nats_max_age_secs,
                })?)
            }
        };

        let log_format = match cli.log_format.as_deref() {
            Some("json") => LogFormat::Json,
            _ => LogFormat::Pretty,
        };

        let qdrant = ServerQdrantConfig::from_parts(
            cli.qdrant_url,
            cli.qdrant_api_key,
            cli.qdrant_timeout_ms.as_deref(),
            cli.qdrant_connect_timeout_ms.as_deref(),
        )?;
        let embedder = EmbedderConfig::from_parts(EmbedderParts {
            endpoint_url: cli.embedding_endpoint_url,
            model: cli.embedding_model,
            api_key: cli.embedding_api_key,
            dimensions: cli.embedding_dimensions,
            batch_size: cli.embedding_batch_size,
            timeout_ms: cli.embedding_timeout_ms,
            max_retries: cli.embedding_max_retries,
            max_input_tokens: cli.embedding_max_input_tokens,
        })?;

        let webdav_username = cli.webdav_username.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_WEBDAV_USERNAME")),
        })?;
        if webdav_username.is_empty() {
            return Err(Error::Config {
                message: format!(
                    "{} is required and must not be empty",
                    setting("NOTEDTHAT_WEBDAV_USERNAME")
                ),
            });
        }

        let webdav_password = cli.webdav_password.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_WEBDAV_PASSWORD")),
        })?;
        if webdav_password.is_empty() {
            return Err(Error::Config {
                message: format!(
                    "{} is required and must not be empty",
                    setting("NOTEDTHAT_WEBDAV_PASSWORD")
                ),
            });
        }

        let mcp_http_allowed_origins =
            comma_list(cli.mcp_http_allowed_origins.as_deref(), &["null"]);
        let mcp_http_allowed_hosts = comma_list(
            cli.mcp_http_allowed_hosts.as_deref(),
            &["127.0.0.1", "localhost", "::1"],
        );
        let mcp_anonymous = parse_mcp_anonymous(cli.mcp_anonymous.as_deref())?;

        let max_patchable_size = cli
            .max_patchable_size
            .unwrap_or_else(|| (100 * 1024 * 1024u64).to_string())
            .parse::<u64>()
            .map_err(|_e: std::num::ParseIntError| Error::Config {
                message: format!(
                    "{} must be a valid u64 integer",
                    setting("NOTEDTHAT_MAX_PATCHABLE_SIZE")
                ),
            })?;
        if max_patchable_size == 0 {
            return Err(Error::Config {
                message: format!("{} must be > 0", setting("NOTEDTHAT_MAX_PATCHABLE_SIZE")),
            });
        }
        if max_patchable_size > MAX_UPLOAD_BYTES {
            return Err(Error::Config {
                message: format!(
                    "{} must not exceed MAX_UPLOAD_BYTES (5 GiB)",
                    setting("NOTEDTHAT_MAX_PATCHABLE_SIZE")
                ),
            });
        }

        let mcp_max_read_bytes = cli
            .mcp_max_read_bytes
            .unwrap_or_else(|| notedthat_mcp::DEFAULT_MAX_READ_BYTES.to_string())
            .parse::<u64>()
            .map_err(|_e: std::num::ParseIntError| Error::Config {
                message: format!(
                    "{} must be a valid u64 integer",
                    setting("NOTEDTHAT_MCP_MAX_READ_BYTES")
                ),
            })?;
        if mcp_max_read_bytes == 0 {
            return Err(Error::Config {
                message: format!("{} must be > 0", setting("NOTEDTHAT_MCP_MAX_READ_BYTES")),
            });
        }

        let staging =
            StagingConfig::from_setting(cli.upload_tmp_dir).map_err(|error| Error::Config {
                message: error.to_string(),
            })?;

        Ok(Self {
            api_token,
            kbs,
            tenant_slug,
            listen_addr,
            storage,
            events,
            log_format,
            qdrant,
            embedder,
            webdav_username,
            webdav_password,
            mcp_http_allowed_origins,
            mcp_http_allowed_hosts,
            mcp_anonymous,
            max_patchable_size,
            mcp_max_read_bytes,
            staging,
            oidc,
        })
    }
}

/// The `NOTEDTHAT_OIDC_*` settings that only mean something once an issuer is set.
fn oidc_dependent_settings(cli: &ServerCli) -> [(&'static str, bool); 6] {
    [
        ("NOTEDTHAT_OIDC_AUDIENCE", cli.oidc_audience.is_some()),
        (
            "NOTEDTHAT_OIDC_USERNAME_CLAIM",
            cli.oidc_username_claim.is_some(),
        ),
        (
            "NOTEDTHAT_OIDC_GROUPS_CLAIM",
            cli.oidc_groups_claim.is_some(),
        ),
        (
            "NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS",
            cli.oidc_http_timeout_ms.is_some(),
        ),
        ("NOTEDTHAT_OIDC_RESOURCE", cli.oidc_resource.is_some()),
        ("NOTEDTHAT_OIDC_CA_CERT", cli.oidc_ca_cert.is_some()),
    ]
}

/// Parse the identity-provider settings.
///
/// `NOTEDTHAT_OIDC_ISSUER` is the switch. Without it, any other `NOTEDTHAT_OIDC_*`
/// setting is refused rather than ignored, for the same reason a setting of
/// the unselected storage backend is: a deployment that sets an audience and
/// no issuer believed it had configured identity tokens, and silently running
/// without them is the wrong way to find out.
fn parse_oidc(cli: &ServerCli) -> Result<Option<OidcSettings>, Error> {
    let Some(issuer) = cli.oidc_issuer.as_deref().map(str::trim) else {
        let offenders: Vec<String> = oidc_dependent_settings(cli)
            .into_iter()
            .filter(|(_, supplied)| *supplied)
            .map(|(name, _)| setting(name))
            .collect();
        if offenders.is_empty() {
            return Ok(None);
        }
        return Err(Error::Config {
            message: format!(
                "{} is unset, so identity tokens are not accepted, but {} {} set; set the \
                 issuer or unset {}",
                setting("NOTEDTHAT_OIDC_ISSUER"),
                offenders.join(", "),
                if offenders.len() == 1 { "is" } else { "are" },
                if offenders.len() == 1 { "it" } else { "them" },
            ),
        });
    };

    let issuer_url = absolute_http_url("NOTEDTHAT_OIDC_ISSUER", issuer)?;
    let audiences = comma_list(cli.oidc_audience.as_deref(), &[]);
    if audiences.is_empty() {
        return Err(Error::Config {
            message: format!(
                "{} is required when {} is set: name the audience the provider puts in \
                 its tokens, usually the client id",
                setting("NOTEDTHAT_OIDC_AUDIENCE"),
                setting("NOTEDTHAT_OIDC_ISSUER"),
            ),
        });
    }
    let claim = |var: &str, supplied: Option<&str>, default: &str| -> Result<String, Error> {
        match supplied.map(str::trim) {
            None => Ok(default.to_string()),
            Some("") => Err(Error::Config {
                message: format!("{} must not be empty", setting(var)),
            }),
            Some(name) => Ok(name.to_string()),
        }
    };
    let username_claim = claim(
        "NOTEDTHAT_OIDC_USERNAME_CLAIM",
        cli.oidc_username_claim.as_deref(),
        OidcSettings::DEFAULT_USERNAME_CLAIM,
    )?;
    let groups_claim = claim(
        "NOTEDTHAT_OIDC_GROUPS_CLAIM",
        cli.oidc_groups_claim.as_deref(),
        OidcSettings::DEFAULT_GROUPS_CLAIM,
    )?;
    let http_timeout = Duration::from_millis(parse_millis(
        "NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS",
        cli.oidc_http_timeout_ms.as_deref(),
        OidcSettings::DEFAULT_HTTP_TIMEOUT_MS,
    )?);
    let resource = cli
        .oidc_resource
        .as_deref()
        .map(str::trim)
        .map(|resource| absolute_http_url("NOTEDTHAT_OIDC_RESOURCE", resource))
        .transpose()?
        .map(|url| url.to_string().trim_end_matches('/').to_string());

    let ca_cert = match cli.oidc_ca_cert.as_deref() {
        None => None,
        Some(path) if path.is_empty() => {
            return Err(Error::Config {
                message: format!("{} must not be empty", setting("NOTEDTHAT_OIDC_CA_CERT")),
            });
        }
        Some(path) => {
            let path = std::path::PathBuf::from(path);
            if !path.is_file() {
                return Err(Error::Config {
                    message: format!(
                        "{} is not a readable file: {}",
                        setting("NOTEDTHAT_OIDC_CA_CERT"),
                        path.display()
                    ),
                });
            }
            Some(path)
        }
    };

    Ok(Some(OidcSettings {
        issuer: issuer_url.to_string(),
        audiences,
        username_claim,
        groups_claim,
        http_timeout,
        resource,
        ca_cert,
    }))
}

/// Parse an `http(s)` URL setting, keeping the operator's spelling.
///
/// Returns the parsed URL only to prove it parses; the `Display` of a parsed
/// URL can differ from the input (a bare origin gains a trailing slash), and
/// the issuer has to be compared byte-for-byte with the provider's `iss`.
fn absolute_http_url(var: &str, raw: &str) -> Result<UrlSpelling, Error> {
    let parsed = url::Url::parse(raw).map_err(|error| Error::Config {
        message: format!("{} is not an absolute URL: {error}", setting(var)),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Config {
            message: format!("{} must use http or https", setting(var)),
        });
    }
    Ok(UrlSpelling(raw.to_string()))
}

/// A URL that parsed, kept in the operator's own spelling.
struct UrlSpelling(String);

impl std::fmt::Display for UrlSpelling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Split a comma-separated allowlist, falling back to `default` when nothing usable
/// was supplied.
///
/// An empty or whitespace-only value means "not configured" rather than "allow
/// nothing", because both defaults here are the safe, loopback-only ones.
fn comma_list(supplied: Option<&str>, default: &[&str]) -> Vec<String> {
    match supplied {
        Some(s) if !s.trim().is_empty() => s
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect(),
        _ => default.iter().map(|v| (*v).to_string()).collect(),
    }
}

/// Qdrant client configuration.
#[derive(Debug, Clone)]
pub struct ServerQdrantConfig {
    /// Qdrant gRPC/HTTP endpoint (`NOTEDTHAT_QDRANT_URL`; required).
    pub url: String,
    /// Optional Qdrant API key (`NOTEDTHAT_QDRANT_API_KEY`).
    pub api_key: Option<String>,
    /// Per-RPC timeout in milliseconds (`NOTEDTHAT_QDRANT_TIMEOUT_MS`; default 30 000).
    ///
    /// `qdrant-client`'s own default is 5 s, which is too tight for a full
    /// embedding batch upserted with `wait(true)`.
    pub timeout_ms: u64,
    /// Connection-establishment timeout in milliseconds
    /// (`NOTEDTHAT_QDRANT_CONNECT_TIMEOUT_MS`; default 10 000).
    pub connect_timeout_ms: u64,
}

impl ServerQdrantConfig {
    /// Validate the Qdrant settings this run supplied.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` if the URL is missing or a timeout is
    /// not a positive integer.
    fn from_parts(
        url: Option<String>,
        api_key: Option<String>,
        timeout_ms: Option<&str>,
        connect_timeout_ms: Option<&str>,
    ) -> Result<Self, Error> {
        let url = url.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("NOTEDTHAT_QDRANT_URL")),
        })?;
        Ok(Self {
            url,
            api_key,
            timeout_ms: parse_millis("NOTEDTHAT_QDRANT_TIMEOUT_MS", timeout_ms, 30_000)?,
            connect_timeout_ms: parse_millis(
                "NOTEDTHAT_QDRANT_CONNECT_TIMEOUT_MS",
                connect_timeout_ms,
                10_000,
            )?,
        })
    }
}

/// Parse a millisecond duration, rejecting zero.
fn parse_millis(var: &str, supplied: Option<&str>, default: u64) -> Result<u64, Error> {
    let Some(raw) = supplied else {
        return Ok(default);
    };
    let value = raw.parse::<u64>().map_err(|_| Error::Config {
        message: format!("{} must be a valid u64 integer", setting(var)),
    })?;
    if value == 0 {
        return Err(Error::Config {
            message: format!("{} must be > 0", setting(var)),
        });
    }
    Ok(value)
}

/// The raw embedder settings, before validation.
///
/// A struct rather than eight positional arguments, because eight `Option<String>`
/// parameters in a row is a call site nothing can typecheck.
struct EmbedderParts {
    endpoint_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    dimensions: Option<String>,
    batch_size: Option<String>,
    timeout_ms: Option<String>,
    max_retries: Option<String>,
    max_input_tokens: Option<String>,
}

/// Embedder configuration.
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
    /// Validate the embedder settings this run supplied.
    ///
    /// # Errors
    ///
    /// Returns `Err(Error::Config { .. })` if any required setting is missing or invalid.
    fn from_parts(parts: EmbedderParts) -> Result<Self, Error> {
        let endpoint_url = parts.endpoint_url.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("EMBEDDING_ENDPOINT_URL")),
        })?;
        let model = parts.model.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("EMBEDDING_MODEL")),
        })?;
        let api_key = parts.api_key.ok_or_else(|| Error::Config {
            message: format!("{} is required", setting("EMBEDDING_API_KEY")),
        })?;
        let dimensions = parse_number("EMBEDDING_DIMENSIONS", parts.dimensions.as_deref())?
            .ok_or_else(|| Error::Config {
                message: format!("{} is required", setting("EMBEDDING_DIMENSIONS")),
            })?;
        Ok(Self {
            endpoint_url,
            model,
            api_key,
            dimensions,
            batch_size: parse_number("EMBEDDING_BATCH_SIZE", parts.batch_size.as_deref())?
                .unwrap_or(32),
            timeout_ms: parse_number("EMBEDDING_TIMEOUT_MS", parts.timeout_ms.as_deref())?
                .unwrap_or(30_000),
            max_retries: parse_number("EMBEDDING_MAX_RETRIES", parts.max_retries.as_deref())?
                .unwrap_or(3),
            max_input_tokens: parse_number(
                "EMBEDDING_MAX_INPUT_TOKENS",
                parts.max_input_tokens.as_deref(),
            )?
            .unwrap_or(8192),
        })
    }
}

/// Parse an optional integer setting, naming it on failure.
fn parse_number<T>(var: &str, supplied: Option<&str>) -> Result<Option<T>, Error>
where
    T: std::str::FromStr<Err = std::num::ParseIntError>,
{
    supplied
        .map(|raw| {
            raw.parse::<T>().map_err(|e| Error::Config {
                message: format!("{} is invalid: {e}", setting(var)),
            })
        })
        .transpose()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const ALL_ENV_KEYS: [&str; 51] = [
        "NOTEDTHAT_API_TOKEN",
        "NOTEDTHAT_KBS",
        "NOTEDTHAT_STORAGE_BACKEND",
        "NOTEDTHAT_FS_ROOT",
        "NOTEDTHAT_FS_METADATA",
        "NOTEDTHAT_FS_FILE_MODE",
        "NOTEDTHAT_FS_DIR_MODE",
        "NOTEDTHAT_FS_ALLOW_LOSSY_NAMES",
        "NOTEDTHAT_FS_WATCH",
        "NOTEDTHAT_FS_WATCH_DEBOUNCE_MS",
        "NOTEDTHAT_S3_REGION",
        "NOTEDTHAT_S3_ACCESS_KEY_ID",
        "NOTEDTHAT_S3_SECRET_ACCESS_KEY",
        "NOTEDTHAT_LISTEN_ADDR",
        "NOTEDTHAT_LOG_FORMAT",
        "NOTEDTHAT_S3_ENDPOINT_URL",
        "NOTEDTHAT_S3_FORCE_PATH_STYLE",
        "NOTEDTHAT_EVENTS_BACKEND",
        "NOTEDTHAT_EVENTS_MEMORY_CAPACITY",
        "NOTEDTHAT_NATS_URL",
        "NOTEDTHAT_NATS_STREAM",
        "NOTEDTHAT_NATS_MAX_AGE_SECS",
        "NOTEDTHAT_QDRANT_URL",
        "NOTEDTHAT_QDRANT_API_KEY",
        "NOTEDTHAT_QDRANT_TIMEOUT_MS",
        "NOTEDTHAT_QDRANT_CONNECT_TIMEOUT_MS",
        "NOTEDTHAT_WEBDAV_USERNAME",
        "NOTEDTHAT_WEBDAV_PASSWORD",
        "NOTEDTHAT_WEBDAV_LISTEN_ADDR",
        "NOTEDTHAT_MCP_HTTP_BIND",
        "NOTEDTHAT_MCP_HTTP_ENABLED",
        "NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS",
        "NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS",
        "NOTEDTHAT_MCP_ANONYMOUS",
        "NOTEDTHAT_MCP_MAX_READ_BYTES",
        "NOTEDTHAT_MAX_PATCHABLE_SIZE",
        "NOTEDTHAT_UPLOAD_TMP_DIR",
        "NOTEDTHAT_OIDC_ISSUER",
        "NOTEDTHAT_OIDC_AUDIENCE",
        "NOTEDTHAT_OIDC_USERNAME_CLAIM",
        "NOTEDTHAT_OIDC_GROUPS_CLAIM",
        "NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS",
        "NOTEDTHAT_OIDC_RESOURCE",
        "NOTEDTHAT_OIDC_CA_CERT",
        "EMBEDDING_ENDPOINT_URL",
        "EMBEDDING_MODEL",
        "EMBEDDING_API_KEY",
        "EMBEDDING_DIMENSIONS",
        "EMBEDDING_BATCH_SIZE",
        "EMBEDDING_TIMEOUT_MS",
        "EMBEDDING_MAX_RETRIES",
        "EMBEDDING_MAX_INPUT_TOKENS",
    ];

    /// A configuration diagnostic has to be actionable from either direction, so
    /// it names the environment variable, the flag that overrides it, and what is
    /// wrong. Asserting on all three at once keeps the check readable while making
    /// it stricter than a single `contains`.
    fn names_setting(message: &str, env_var: &str, complaint: &str) -> bool {
        message.contains(env_var)
            && message.contains(&notedthat_core::flag_for(env_var))
            && message.contains(complaint)
    }

    fn run_with_env<F: FnOnce() -> R, R>(overrides: &[(&str, Option<&str>)], f: F) -> R {
        let mut vars: Vec<(&str, Option<&str>)> = vec![
            ("NOTEDTHAT_API_TOKEN", Some("test-token")),
            ("NOTEDTHAT_KBS", Some("notes,docs")),
            ("NOTEDTHAT_STORAGE_BACKEND", None),
            ("NOTEDTHAT_FS_ROOT", None),
            ("NOTEDTHAT_FS_METADATA", None),
            ("NOTEDTHAT_FS_FILE_MODE", None),
            ("NOTEDTHAT_FS_DIR_MODE", None),
            ("NOTEDTHAT_FS_ALLOW_LOSSY_NAMES", None),
            ("NOTEDTHAT_S3_REGION", Some("us-east-1")),
            ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("key")),
            ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("secret")),
            ("NOTEDTHAT_LISTEN_ADDR", None),
            ("NOTEDTHAT_LOG_FORMAT", None),
            ("NOTEDTHAT_S3_ENDPOINT_URL", None),
            ("NOTEDTHAT_S3_FORCE_PATH_STYLE", None),
            ("NOTEDTHAT_EVENTS_BACKEND", None),
            ("NOTEDTHAT_EVENTS_MEMORY_CAPACITY", None),
            ("NOTEDTHAT_NATS_URL", None),
            ("NOTEDTHAT_NATS_STREAM", None),
            ("NOTEDTHAT_NATS_MAX_AGE_SECS", None),
            ("NOTEDTHAT_QDRANT_URL", Some("http://localhost:6334")),
            ("NOTEDTHAT_QDRANT_API_KEY", None),
            ("NOTEDTHAT_QDRANT_TIMEOUT_MS", None),
            ("NOTEDTHAT_QDRANT_CONNECT_TIMEOUT_MS", None),
            ("NOTEDTHAT_WEBDAV_USERNAME", Some("webdav-user")),
            ("NOTEDTHAT_WEBDAV_PASSWORD", Some("webdav-pass")),
            ("NOTEDTHAT_WEBDAV_LISTEN_ADDR", None),
            ("NOTEDTHAT_MCP_HTTP_BIND", None),
            ("NOTEDTHAT_MCP_HTTP_ENABLED", None),
            ("NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS", None),
            ("NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS", None),
            ("NOTEDTHAT_MCP_ANONYMOUS", None),
            ("NOTEDTHAT_MCP_MAX_READ_BYTES", None),
            ("NOTEDTHAT_MAX_PATCHABLE_SIZE", None),
            ("NOTEDTHAT_UPLOAD_TMP_DIR", None),
            ("NOTEDTHAT_OIDC_ISSUER", None),
            ("NOTEDTHAT_OIDC_AUDIENCE", None),
            ("NOTEDTHAT_OIDC_USERNAME_CLAIM", None),
            ("NOTEDTHAT_OIDC_GROUPS_CLAIM", None),
            ("NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS", None),
            ("NOTEDTHAT_OIDC_RESOURCE", None),
            ("NOTEDTHAT_OIDC_CA_CERT", None),
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
    fn test_default_staging_directory() {
        let cfg = run_with_env(&[], Config::from_env).unwrap();
        assert_eq!(cfg.staging.directory(), std::env::temp_dir());
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
    fn removed_listener_variables_are_rejected_with_their_replacement() {
        for (key, replacement) in REMOVED_LISTENER_ENV_VARS {
            let result = run_with_env(&[(key, Some("some-stale-value"))], Config::from_env);
            let message = result.map_or_else(
                |e| e.to_string(),
                |_| panic!("{key} must be rejected at startup"),
            );

            assert!(message.contains(key), "{key} error must name the variable");
            assert!(
                message.contains(replacement),
                "{key} error must name its replacement"
            );
        }
    }

    #[test]
    fn removed_listener_variables_are_rejected_even_when_empty() {
        let result = run_with_env(
            &[("NOTEDTHAT_MCP_HTTP_ENABLED", Some(""))],
            Config::from_env,
        );

        assert!(
            result.is_err(),
            "an empty removed variable is still an explicit operator setting"
        );
    }

    #[test]
    fn unset_removed_listener_variables_leave_the_default_listener() {
        let config = run_with_env(&[], Config::from_env)
            .expect("configuration must parse when no removed variable is set");

        assert_eq!(config.listen_addr.to_string(), "0.0.0.0:8080");
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

    /// The inventory is what `cli::tests::every_setting_has_both_a_flag_and_a_variable`
    /// checks the parser against, so a setting missing from here is a setting that
    /// can silently lose its flag.
    #[test]
    fn all_env_keys_are_accounted_for() {
        assert_eq!(ALL_ENV_KEYS.len(), 51);
    }

    #[test]
    fn mcp_max_read_bytes_defaults_to_the_api_body_cap() {
        let cfg =
            run_with_env(&[("NOTEDTHAT_MCP_MAX_READ_BYTES", None)], Config::from_env).unwrap();
        assert_eq!(cfg.mcp_max_read_bytes, 16 * 1024 * 1024);
    }

    #[test]
    fn mcp_max_read_bytes_accepts_explicit_bytes() {
        let cfg = run_with_env(
            &[("NOTEDTHAT_MCP_MAX_READ_BYTES", Some("4096"))],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.mcp_max_read_bytes, 4096);
    }

    #[test]
    fn mcp_max_read_bytes_rejects_zero_and_non_numbers() {
        for (value, fragment) in [("0", "must be > 0"), ("lots", "must be a valid u64")] {
            let result = run_with_env(
                &[("NOTEDTHAT_MCP_MAX_READ_BYTES", Some(value))],
                Config::from_env,
            );
            assert!(matches!(result, Err(Error::Config { .. })));
            assert!(names_setting(
                &result.unwrap_err().to_string(),
                "NOTEDTHAT_MCP_MAX_READ_BYTES",
                fragment
            ));
        }
    }

    #[test]
    fn max_patchable_size_defaults_to_100_mib() {
        let cfg =
            run_with_env(&[("NOTEDTHAT_MAX_PATCHABLE_SIZE", None)], Config::from_env).unwrap();
        assert_eq!(cfg.max_patchable_size, 100 * 1024 * 1024);
    }

    #[test]
    fn max_patchable_size_accepts_explicit_bytes() {
        let cfg = run_with_env(
            &[("NOTEDTHAT_MAX_PATCHABLE_SIZE", Some("52428800"))],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.max_patchable_size, 50 * 1024 * 1024);
    }

    #[test]
    fn max_patchable_size_rejects_zero() {
        let result = run_with_env(
            &[("NOTEDTHAT_MAX_PATCHABLE_SIZE", Some("0"))],
            Config::from_env,
        );
        assert!(matches!(result, Err(Error::Config { .. })));
        assert!(names_setting(
            &result.unwrap_err().to_string(),
            "NOTEDTHAT_MAX_PATCHABLE_SIZE",
            "must be > 0"
        ));
    }

    #[test]
    fn max_patchable_size_rejects_values_over_max_upload_bytes() {
        let result = run_with_env(
            &[("NOTEDTHAT_MAX_PATCHABLE_SIZE", Some("6442450944"))],
            Config::from_env,
        );
        assert!(matches!(result, Err(Error::Config { .. })));
        assert!(names_setting(
            &result.unwrap_err().to_string(),
            "NOTEDTHAT_MAX_PATCHABLE_SIZE",
            "must not exceed MAX_UPLOAD_BYTES (5 GiB)"
        ));
    }

    #[test]
    fn max_patchable_size_rejects_non_numeric_values() {
        let result = run_with_env(
            &[("NOTEDTHAT_MAX_PATCHABLE_SIZE", Some("not-a-number"))],
            Config::from_env,
        );
        assert!(matches!(result, Err(Error::Config { .. })));
        assert!(names_setting(
            &result.unwrap_err().to_string(),
            "NOTEDTHAT_MAX_PATCHABLE_SIZE",
            "must be a valid u64 integer"
        ));
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
            assert_eq!(cfg.mcp_http_allowed_origins, vec!["null"]);
            assert_eq!(
                cfg.mcp_http_allowed_hosts,
                vec!["127.0.0.1", "localhost", "::1"]
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
        fn mcp_anonymous_defaults_to_auto() {
            let cfg = run_with_env(&[], Config::from_env).unwrap();
            assert_eq!(cfg.mcp_anonymous, McpAnonymous::Auto);
        }

        #[test]
        fn mcp_anonymous_empty_or_blank_is_auto_like_its_siblings() {
            // Compose hands every MCP variable through as `${VAR-}`, so an
            // unset variable arrives as an empty string.
            for value in ["", "   "] {
                let cfg = run_with_env(
                    &[("NOTEDTHAT_MCP_ANONYMOUS", Some(value))],
                    Config::from_env,
                )
                .unwrap();
                assert_eq!(cfg.mcp_anonymous, McpAnonymous::Auto, "{value:?}");
            }
        }

        #[test]
        fn mcp_anonymous_never_in_any_case() {
            for value in ["never", "NEVER", " Never "] {
                let cfg = run_with_env(
                    &[("NOTEDTHAT_MCP_ANONYMOUS", Some(value))],
                    Config::from_env,
                )
                .unwrap();
                assert_eq!(cfg.mcp_anonymous, McpAnonymous::Never, "{value:?}");
            }
        }

        #[test]
        fn an_unknown_mcp_anonymous_mode_is_refused_rather_than_defaulted() {
            let err = run_with_env(
                &[("NOTEDTHAT_MCP_ANONYMOUS", Some("nevr"))],
                Config::from_env,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("NOTEDTHAT_MCP_ANONYMOUS") && err.contains("nevr"),
                "names the setting and the value: {err}"
            );
        }

        #[test]
        fn mcp_http_with_empty_token_fails() {
            let result = run_with_env(&[("NOTEDTHAT_API_TOKEN", Some(""))], Config::from_env);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("NOTEDTHAT_API_TOKEN"),
                "error should mention NOTEDTHAT_API_TOKEN: {msg}"
            );
        }

        #[test]
        fn mcp_http_with_whitespace_token_fails() {
            let result = run_with_env(&[("NOTEDTHAT_API_TOKEN", Some("   "))], Config::from_env);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("NOTEDTHAT_API_TOKEN"),
                "error should mention NOTEDTHAT_API_TOKEN: {msg}"
            );
        }
    }

    mod storage_backend {
        use super::*;

        #[test]
        fn the_default_is_s3_so_existing_deployments_are_unaffected() {
            run_with_env(&[], || {
                let config = Config::from_env().expect("valid");
                assert_eq!(config.storage.kind(), StorageBackendKind::S3);
            });
        }

        #[test]
        fn selecting_fs_reads_the_fs_variables_and_stops_requiring_s3() {
            run_with_env(
                &[
                    ("NOTEDTHAT_STORAGE_BACKEND", Some("fs")),
                    ("NOTEDTHAT_FS_ROOT", Some("/srv/notedthat")),
                    ("NOTEDTHAT_S3_REGION", None),
                    ("NOTEDTHAT_S3_ACCESS_KEY_ID", None),
                    ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", None),
                ],
                || {
                    let config = Config::from_env().expect("valid");
                    assert_eq!(config.storage.kind(), StorageBackendKind::Fs);
                },
            );
        }

        /// Unlike `NOTEDTHAT_LOG_FORMAT`, a typo here must not fall back — it would
        /// silently point the server at a different store.
        #[test]
        fn an_unknown_backend_is_refused_rather_than_defaulted() {
            run_with_env(&[("NOTEDTHAT_STORAGE_BACKEND", Some("filesystem"))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("expected \"s3\" or \"fs\""), "{error}");
                assert!(error.contains("filesystem"), "{error}");
            });
        }

        #[test]
        fn an_empty_backend_selector_is_refused() {
            run_with_env(&[("NOTEDTHAT_STORAGE_BACKEND", Some(""))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(error.contains("must not be empty"), "{error}");
            });
        }

        #[test]
        fn selecting_fs_without_a_root_names_the_variable() {
            run_with_env(
                &[
                    ("NOTEDTHAT_STORAGE_BACKEND", Some("fs")),
                    ("NOTEDTHAT_S3_REGION", None),
                    ("NOTEDTHAT_S3_ACCESS_KEY_ID", None),
                    ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", None),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(
                        names_setting(&error, "NOTEDTHAT_FS_ROOT", "is required"),
                        "{error}"
                    );
                },
            );
        }

        #[test]
        fn leftover_s3_variables_under_fs_are_reported_together() {
            run_with_env(
                &[
                    ("NOTEDTHAT_STORAGE_BACKEND", Some("fs")),
                    ("NOTEDTHAT_FS_ROOT", Some("/srv/notedthat")),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(error.contains("belong to the s3 backend"), "{error}");
                    // All of them at once, not one per restart.
                    assert!(error.contains("NOTEDTHAT_S3_REGION"), "{error}");
                    assert!(error.contains("NOTEDTHAT_S3_ACCESS_KEY_ID"), "{error}");
                    assert!(error.contains("NOTEDTHAT_S3_SECRET_ACCESS_KEY"), "{error}");
                    assert!(error.contains("NOTEDTHAT_STORAGE_BACKEND=s3"), "{error}");
                },
            );
        }

        /// The case this check exists for: the operator sets a root and forgets the
        /// selector, and would otherwise get a healthy S3 deployment with an unread root.
        #[test]
        fn an_fs_root_without_the_selector_is_refused_and_says_why() {
            run_with_env(&[("NOTEDTHAT_FS_ROOT", Some("/srv/notedthat"))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(
                    names_setting(&error, "NOTEDTHAT_STORAGE_BACKEND", "is unset"),
                    "{error}"
                );
                assert!(error.contains("NOTEDTHAT_FS_ROOT"), "{error}");
                assert!(error.contains("NOTEDTHAT_STORAGE_BACKEND=fs"), "{error}");
            });
        }

        /// An empty value is still a value — matching how removed variables are checked.
        #[test]
        fn an_empty_cross_backend_variable_still_counts() {
            run_with_env(
                &[
                    ("NOTEDTHAT_STORAGE_BACKEND", Some("fs")),
                    ("NOTEDTHAT_FS_ROOT", Some("/srv/notedthat")),
                    ("NOTEDTHAT_S3_REGION", Some("")),
                    ("NOTEDTHAT_S3_ACCESS_KEY_ID", None),
                    ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", None),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(error.contains("NOTEDTHAT_S3_REGION"), "{error}");
                },
            );
        }

        /// The rejection table is checked against each adapter's own inventory, so it
        /// cannot drift from what those adapters actually read.
        #[test]
        fn the_rejection_table_matches_each_adapter_inventory() {
            let table = backend_owned_settings(&ServerCli::default());
            let s3: Vec<&str> = table
                .iter()
                .filter(|(_, kind, _)| *kind == StorageBackendKind::S3)
                .map(|(name, _, _)| *name)
                .collect();
            let fs: Vec<&str> = table
                .iter()
                .filter(|(_, kind, _)| *kind == StorageBackendKind::Fs)
                .map(|(name, _, _)| *name)
                .collect();
            assert_eq!(s3, notedthat_storage_s3::S3_ENV_VARS.to_vec());
            assert_eq!(fs, notedthat_storage_fs::FS_ENV_VARS.to_vec());

            for (name, _, _) in &table {
                assert!(
                    ALL_ENV_KEYS.contains(name),
                    "{name} is read but missing from ALL_ENV_KEYS"
                );
            }
        }

        /// A default `ServerCli` supplies nothing, so nothing can be an offender —
        /// the guard against a field being wired to the wrong entry in the table.
        #[test]
        fn nothing_is_supplied_by_a_default_command_line() {
            assert!(
                backend_owned_settings(&ServerCli::default())
                    .iter()
                    .all(|(_, _, supplied)| !supplied)
            );
        }
    }

    mod events_backend {
        use super::*;

        #[test]
        fn the_default_is_none_so_existing_deployments_publish_nothing() {
            run_with_env(&[], || {
                let config = Config::from_env().expect("valid");
                assert_eq!(config.events.kind(), EventsBackendKind::None);
            });
        }

        #[test]
        fn memory_reads_its_capacity_and_defaults_it() {
            run_with_env(&[("NOTEDTHAT_EVENTS_BACKEND", Some("memory"))], || {
                let config = Config::from_env().expect("valid");
                match config.events {
                    EventsConfig::Memory(memory) => {
                        assert_eq!(memory.capacity, notedthat_events::DEFAULT_MEMORY_CAPACITY);
                    }
                    other => panic!("expected memory, got {other:?}"),
                }
            });
            run_with_env(
                &[
                    ("NOTEDTHAT_EVENTS_BACKEND", Some("memory")),
                    ("NOTEDTHAT_EVENTS_MEMORY_CAPACITY", Some("250")),
                ],
                || {
                    let config = Config::from_env().expect("valid");
                    match config.events {
                        EventsConfig::Memory(memory) => assert_eq!(memory.capacity, 250),
                        other => panic!("expected memory, got {other:?}"),
                    }
                },
            );
        }

        #[test]
        fn nats_requires_its_url_and_names_the_variable() {
            run_with_env(&[("NOTEDTHAT_EVENTS_BACKEND", Some("nats"))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(
                    names_setting(&error, "NOTEDTHAT_NATS_URL", "is required"),
                    "{error}"
                );
            });
            run_with_env(
                &[
                    ("NOTEDTHAT_EVENTS_BACKEND", Some("nats")),
                    ("NOTEDTHAT_NATS_URL", Some("nats://broker:4222")),
                    ("NOTEDTHAT_NATS_STREAM", Some("evt")),
                    ("NOTEDTHAT_NATS_MAX_AGE_SECS", Some("60")),
                ],
                || {
                    let config = Config::from_env().expect("valid");
                    match config.events {
                        EventsConfig::Nats(nats) => {
                            assert_eq!(nats.url, "nats://broker:4222");
                            assert_eq!(nats.stream, "evt");
                            assert_eq!(nats.max_age, Duration::from_secs(60));
                        }
                        other => panic!("expected nats, got {other:?}"),
                    }
                },
            );
        }

        /// A typo must not fall back to `none`: the server would start and simply
        /// never announce a change.
        #[test]
        fn an_unknown_events_backend_is_refused_rather_than_defaulted() {
            run_with_env(&[("NOTEDTHAT_EVENTS_BACKEND", Some("kafka"))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(
                    error.contains("expected \"none\", \"memory\" or \"nats\""),
                    "{error}"
                );
                assert!(error.contains("kafka"), "{error}");
            });
            run_with_env(&[("NOTEDTHAT_EVENTS_BACKEND", Some(""))], || {
                let error = Config::from_env().unwrap_err().to_string();
                assert!(
                    names_setting(&error, "NOTEDTHAT_EVENTS_BACKEND", "must not be empty"),
                    "{error}"
                );
            });
        }

        #[test]
        fn nats_variables_under_memory_are_reported_together_with_the_fix() {
            run_with_env(
                &[
                    ("NOTEDTHAT_EVENTS_BACKEND", Some("memory")),
                    ("NOTEDTHAT_NATS_URL", Some("nats://broker:4222")),
                    ("NOTEDTHAT_NATS_STREAM", Some("evt")),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(
                        names_setting(&error, "NOTEDTHAT_EVENTS_BACKEND", "is memory"),
                        "{error}"
                    );
                    assert!(error.contains("belong to the nats backend"), "{error}");
                    assert!(error.contains("NOTEDTHAT_NATS_URL"), "{error}");
                    assert!(error.contains("NOTEDTHAT_NATS_STREAM"), "{error}");
                    assert!(error.contains("NOTEDTHAT_EVENTS_BACKEND=nats"), "{error}");
                },
            );
        }

        /// The operator configures a broker and forgets the selector: the highest-value
        /// case, since the server would otherwise start healthy and silent.
        #[test]
        fn a_nats_url_without_the_selector_is_refused_and_says_why() {
            run_with_env(
                &[("NOTEDTHAT_NATS_URL", Some("nats://broker:4222"))],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(
                        names_setting(&error, "NOTEDTHAT_EVENTS_BACKEND", "is unset"),
                        "{error}"
                    );
                    assert!(error.contains("default none backend"), "{error}");
                    assert!(error.contains("NOTEDTHAT_EVENTS_BACKEND=nats"), "{error}");
                },
            );
        }

        /// Both unselected backends' variables at once: each owner is named, and
        /// each fix is offered.
        #[test]
        fn offenders_from_two_backends_are_grouped_by_owner() {
            run_with_env(
                &[
                    ("NOTEDTHAT_EVENTS_MEMORY_CAPACITY", Some("10")),
                    ("NOTEDTHAT_NATS_URL", Some("nats://broker:4222")),
                ],
                || {
                    let error = Config::from_env().unwrap_err().to_string();
                    assert!(error.contains("belong to the memory backend"), "{error}");
                    assert!(error.contains("belong to the nats backend"), "{error}");
                    assert!(
                        error.contains(
                            "NOTEDTHAT_EVENTS_BACKEND=memory or NOTEDTHAT_EVENTS_BACKEND=nats"
                        ),
                        "{error}"
                    );
                },
            );
        }

        #[test]
        fn the_rejection_table_matches_each_adapter_inventory() {
            let table = events_owned_settings(&ServerCli::default());
            let memory: Vec<&str> = table
                .iter()
                .filter(|(_, kind, _)| *kind == EventsBackendKind::Memory)
                .map(|(name, _, _)| *name)
                .collect();
            let nats: Vec<&str> = table
                .iter()
                .filter(|(_, kind, _)| *kind == EventsBackendKind::Nats)
                .map(|(name, _, _)| *name)
                .collect();
            assert_eq!(memory, notedthat_events::MEMORY_ENV_VARS.to_vec());
            assert_eq!(nats, notedthat_events::NATS_ENV_VARS.to_vec());

            for (name, _, _) in &table {
                assert!(
                    ALL_ENV_KEYS.contains(name),
                    "{name} is read but missing from ALL_ENV_KEYS"
                );
            }
            assert!(
                table.iter().all(|(_, _, supplied)| !supplied),
                "a default command line supplies nothing"
            );
        }
    }

    mod oidc {
        use super::*;

        const ISSUER: &str = "https://auth.example.com/application/o/notedthat/";

        #[test]
        fn no_oidc_settings_means_no_verifier() {
            let cfg = run_with_env(&[], Config::from_env).expect("valid config");
            assert!(cfg.oidc.is_none());
        }

        #[test]
        fn an_issuer_with_an_audience_enables_oidc_with_the_defaults() {
            let cfg = run_with_env(
                &[
                    ("NOTEDTHAT_OIDC_ISSUER", Some(ISSUER)),
                    ("NOTEDTHAT_OIDC_AUDIENCE", Some("notedthat, mcp-client")),
                ],
                Config::from_env,
            )
            .expect("valid config");
            let oidc = cfg.oidc.expect("configured");
            assert_eq!(oidc.issuer, ISSUER, "the operator's spelling is kept");
            assert_eq!(oidc.audiences, vec!["notedthat", "mcp-client"]);
            assert_eq!(oidc.username_claim, "preferred_username");
            assert_eq!(oidc.groups_claim, "groups");
            assert_eq!(oidc.http_timeout, Duration::from_millis(5000));
            assert_eq!(oidc.resource, None);
            assert_eq!(oidc.ca_cert, None);
            assert_eq!(
                oidc.discovery_url(),
                "https://auth.example.com/application/o/notedthat/.well-known/openid-configuration"
            );
        }

        #[test]
        fn every_oidc_setting_is_read() {
            let cfg = run_with_env(
                &[
                    ("NOTEDTHAT_OIDC_ISSUER", Some("https://auth.example.com")),
                    ("NOTEDTHAT_OIDC_AUDIENCE", Some("notedthat")),
                    ("NOTEDTHAT_OIDC_USERNAME_CLAIM", Some("email")),
                    (
                        "NOTEDTHAT_OIDC_GROUPS_CLAIM",
                        Some("urn:zitadel:iam:org:project:roles"),
                    ),
                    ("NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS", Some("250")),
                    (
                        "NOTEDTHAT_OIDC_RESOURCE",
                        Some("https://notes.example.com/"),
                    ),
                ],
                Config::from_env,
            )
            .expect("valid config");
            let oidc = cfg.oidc.expect("configured");
            assert_eq!(oidc.username_claim, "email");
            assert_eq!(oidc.groups_claim, "urn:zitadel:iam:org:project:roles");
            assert_eq!(oidc.http_timeout, Duration::from_millis(250));
            assert_eq!(
                oidc.resource.as_deref(),
                Some("https://notes.example.com"),
                "the resource is an origin, so its trailing slash is dropped"
            );
        }

        #[test]
        fn oidc_settings_without_an_issuer_are_rejected() {
            let error = run_with_env(
                &[
                    ("NOTEDTHAT_OIDC_AUDIENCE", Some("notedthat")),
                    ("NOTEDTHAT_OIDC_GROUPS_CLAIM", Some("roles")),
                ],
                Config::from_env,
            )
            .expect_err("refused");
            let message = error.to_string();
            assert!(
                names_setting(&message, "NOTEDTHAT_OIDC_ISSUER", "unset"),
                "{message}"
            );
            assert!(message.contains("NOTEDTHAT_OIDC_AUDIENCE"), "{message}");
            assert!(message.contains("NOTEDTHAT_OIDC_GROUPS_CLAIM"), "{message}");
        }

        #[test]
        fn an_issuer_without_an_audience_is_rejected() {
            let error = run_with_env(&[("NOTEDTHAT_OIDC_ISSUER", Some(ISSUER))], Config::from_env)
                .expect_err("refused");
            assert!(
                names_setting(&error.to_string(), "NOTEDTHAT_OIDC_AUDIENCE", "required"),
                "{error}"
            );
        }

        #[test]
        fn an_issuer_that_is_not_an_http_url_is_rejected() {
            for bad in ["auth.example.com", "ldap://auth.example.com", ""] {
                let error = run_with_env(
                    &[
                        ("NOTEDTHAT_OIDC_ISSUER", Some(bad)),
                        ("NOTEDTHAT_OIDC_AUDIENCE", Some("notedthat")),
                    ],
                    Config::from_env,
                )
                .expect_err("refused");
                assert!(
                    error.to_string().contains("NOTEDTHAT_OIDC_ISSUER"),
                    "{bad}: {error}"
                );
            }
        }

        #[test]
        fn an_empty_claim_name_and_a_zero_timeout_are_rejected() {
            for (var, value) in [
                ("NOTEDTHAT_OIDC_USERNAME_CLAIM", " "),
                ("NOTEDTHAT_OIDC_GROUPS_CLAIM", ""),
                ("NOTEDTHAT_OIDC_HTTP_TIMEOUT_MS", "0"),
                ("NOTEDTHAT_OIDC_RESOURCE", "notes.example.com"),
                ("NOTEDTHAT_OIDC_CA_CERT", "/nonexistent/ca.pem"),
            ] {
                let error = run_with_env(
                    &[
                        ("NOTEDTHAT_OIDC_ISSUER", Some(ISSUER)),
                        ("NOTEDTHAT_OIDC_AUDIENCE", Some("notedthat")),
                        (var, Some(value)),
                    ],
                    Config::from_env,
                )
                .expect_err("refused");
                assert!(error.to_string().contains(var), "{var}: {error}");
            }
        }
    }
}
