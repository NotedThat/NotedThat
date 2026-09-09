//! The `notedthat-server` command line.
//!
//! Every setting the server reads is reachable two ways: as an environment
//! variable, and as the long flag named after it. When both are supplied the
//! flag wins — that precedence is `clap`'s, granted by `env = "..."` on each
//! argument, and it is the whole reason this layer exists.
//!
//! What this type deliberately does *not* do is validate. Every field is an
//! `Option` of an unparsed value, so requiredness, ranges, enumerations and
//! cross-setting rules all stay in [`crate::config`], stated once and reached
//! identically from either source. It also keeps the diagnostics: `clap`'s
//! own "the following required arguments were not provided" would replace
//! messages that name the environment variable an operator is looking for.

use clap::Parser;
use std::ffi::OsString;

/// A setting supplied as a bare flag means "true"; `--flag=false` still turns it
/// off, and the environment keeps its strict `true`/`false` parse either way.
const BOOL_FLAG: &str = "true";

/// Command-line arguments, each mirroring one environment variable.
///
/// Construct it with [`clap::Parser::parse`] in a binary, or with
/// [`ServerCli::from_env`] to read the environment alone.
// Every doc comment below is rendered verbatim by `--help`, so it is written for
// a terminal rather than for rustdoc: backticks around `WebDAV` or `SeaweedFS`
// would reach the operator as literal characters.
//
// `--help` shows each setting's current value from the environment, which is a
// useful thing to be able to check — except for a credential, where it would put
// a live secret on the terminal of anyone who asked for help. Those carry
// `hide_env_values`, so their variable is named and their value is not.
#[allow(clippy::doc_markdown)]
#[derive(Parser, Debug, Default, Clone)]
#[command(
    name = "notedthat-server",
    version,
    about = "NotedThat server — HTTP API, WebDAV and remote MCP in one process",
    long_about = "NotedThat server — HTTP API, WebDAV and remote MCP in one process.\n\n\
                  Every setting can be given as the flag shown below or as the environment \
                  variable beside it; the flag wins when both are set.\n\n\
                  Arguments are visible to any user on the host via `ps`, and are recorded in \
                  shell history and in `docker inspect`. Prefer the environment variable for \
                  --api-token, --webdav-password, --s3-secret-access-key, --qdrant-api-key and \
                  --embedding-api-key on a shared machine."
)]
pub struct ServerCli {
    /// Static Bearer token for authenticated API access and every HTTP write.
    #[arg(
        long,
        env = "NOTEDTHAT_API_TOKEN",
        value_name = "TOKEN",
        hide_env_values = true
    )]
    pub api_token: Option<String>,

    /// Comma-separated knowledge base slugs to declare, e.g. `notes,scratch`.
    #[arg(long, env = "NOTEDTHAT_KBS", value_name = "SLUGS")]
    pub kbs: Option<String>,

    /// Address and port the server binds to [default: 0.0.0.0:8080].
    #[arg(long, env = "NOTEDTHAT_LISTEN_ADDR", value_name = "HOST:PORT")]
    pub listen_addr: Option<String>,

    /// Log output format: `pretty` or `json` [default: pretty].
    #[arg(long, env = "NOTEDTHAT_LOG_FORMAT", value_name = "FORMAT")]
    pub log_format: Option<String>,

    /// Largest object eligible for PATCH, in bytes [default: 104857600].
    #[arg(long, env = "NOTEDTHAT_MAX_PATCHABLE_SIZE", value_name = "BYTES")]
    pub max_patchable_size: Option<String>,

    /// Private staging directory for uploads and index snapshots [default: the
    /// platform temporary directory].
    #[arg(long, env = "NOTEDTHAT_UPLOAD_TMP_DIR", value_name = "DIR")]
    pub upload_tmp_dir: Option<OsString>,

    /// HTTP Basic auth username for WebDAV.
    #[arg(long, env = "NOTEDTHAT_WEBDAV_USERNAME", value_name = "USER")]
    pub webdav_username: Option<String>,

    /// HTTP Basic auth password for WebDAV.
    #[arg(
        long,
        env = "NOTEDTHAT_WEBDAV_PASSWORD",
        value_name = "PASSWORD",
        hide_env_values = true
    )]
    pub webdav_password: Option<String>,

    /// Allowed `Origin` values for MCP over HTTP [default: null, i.e. loopback only].
    #[arg(
        long,
        env = "NOTEDTHAT_MCP_HTTP_ALLOWED_ORIGINS",
        value_name = "ORIGINS"
    )]
    pub mcp_http_allowed_origins: Option<String>,

    /// Allowed `Host` values for MCP over HTTP [default: 127.0.0.1,localhost,::1].
    #[arg(long, env = "NOTEDTHAT_MCP_HTTP_ALLOWED_HOSTS", value_name = "HOSTS")]
    pub mcp_http_allowed_hosts: Option<String>,

    /// Object store to run on: `s3` or `fs` [default: s3].
    #[arg(long, env = "NOTEDTHAT_STORAGE_BACKEND", value_name = "BACKEND")]
    pub storage_backend: Option<OsString>,

    /// S3 region. Required with the `s3` backend, even behind a custom endpoint.
    #[arg(long, env = "NOTEDTHAT_S3_REGION", value_name = "REGION")]
    pub s3_region: Option<String>,

    /// S3 access key ID. No ambient credential chain is consulted.
    #[arg(
        long,
        env = "NOTEDTHAT_S3_ACCESS_KEY_ID",
        value_name = "KEY_ID",
        hide_env_values = true
    )]
    pub s3_access_key_id: Option<String>,

    /// S3 secret access key.
    #[arg(
        long,
        env = "NOTEDTHAT_S3_SECRET_ACCESS_KEY",
        value_name = "SECRET",
        hide_env_values = true
    )]
    pub s3_secret_access_key: Option<String>,

    /// Custom S3-compatible endpoint, for SeaweedFS, MinIO, Ceph, Garage or R2.
    #[arg(long, env = "NOTEDTHAT_S3_ENDPOINT_URL", value_name = "URL")]
    pub s3_endpoint_url: Option<String>,

    /// Use path-style addressing, `endpoint/bucket/key` [default: false].
    #[arg(
        long,
        env = "NOTEDTHAT_S3_FORCE_PATH_STYLE",
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = BOOL_FLAG,
    )]
    pub s3_force_path_style: Option<String>,

    /// Absolute path of the storage root. Required with the `fs` backend.
    #[arg(long, env = "NOTEDTHAT_FS_ROOT", value_name = "DIR")]
    pub fs_root: Option<OsString>,

    /// Where per-object metadata is kept [default: sidecar].
    #[arg(long, env = "NOTEDTHAT_FS_METADATA", value_name = "MODE")]
    pub fs_metadata: Option<OsString>,

    /// Octal mode for created object files [default: 0644].
    #[arg(long, env = "NOTEDTHAT_FS_FILE_MODE", value_name = "MODE")]
    pub fs_file_mode: Option<OsString>,

    /// Octal mode for created directories [default: 0755].
    #[arg(long, env = "NOTEDTHAT_FS_DIR_MODE", value_name = "MODE")]
    pub fs_dir_mode: Option<OsString>,

    /// Start even on a filesystem that folds case or normalizes Unicode
    /// [default: false].
    #[arg(
        long,
        env = "NOTEDTHAT_FS_ALLOW_LOSSY_NAMES",
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = BOOL_FLAG,
    )]
    pub fs_allow_lossy_names: Option<OsString>,

    /// Qdrant gRPC endpoint, e.g. `http://127.0.0.1:6334`.
    #[arg(long, env = "NOTEDTHAT_QDRANT_URL", value_name = "URL")]
    pub qdrant_url: Option<String>,

    /// API key for an authenticated Qdrant instance.
    #[arg(
        long,
        env = "NOTEDTHAT_QDRANT_API_KEY",
        value_name = "KEY",
        hide_env_values = true
    )]
    pub qdrant_api_key: Option<String>,

    /// Per-RPC Qdrant timeout in milliseconds [default: 30000].
    #[arg(long, env = "NOTEDTHAT_QDRANT_TIMEOUT_MS", value_name = "MS")]
    pub qdrant_timeout_ms: Option<String>,

    /// Qdrant connection-establishment timeout in milliseconds [default: 10000].
    #[arg(long, env = "NOTEDTHAT_QDRANT_CONNECT_TIMEOUT_MS", value_name = "MS")]
    pub qdrant_connect_timeout_ms: Option<String>,

    /// Base URL of the OpenAI-compatible embedding endpoint.
    #[arg(long, env = "EMBEDDING_ENDPOINT_URL", value_name = "URL")]
    pub embedding_endpoint_url: Option<String>,

    /// Embedding model name, e.g. `text-embedding-3-small`.
    #[arg(long, env = "EMBEDDING_MODEL", value_name = "MODEL")]
    pub embedding_model: Option<String>,

    /// Bearer token for the embedding endpoint.
    #[arg(
        long,
        env = "EMBEDDING_API_KEY",
        value_name = "KEY",
        hide_env_values = true
    )]
    pub embedding_api_key: Option<String>,

    /// Output vector dimensions. Must match the model and is baked into the
    /// Qdrant collection at first provisioning.
    #[arg(long, env = "EMBEDDING_DIMENSIONS", value_name = "N")]
    pub embedding_dimensions: Option<String>,

    /// Text chunks per embedding request [default: 32].
    #[arg(long, env = "EMBEDDING_BATCH_SIZE", value_name = "N")]
    pub embedding_batch_size: Option<String>,

    /// Per-request embedding HTTP timeout in milliseconds [default: 30000].
    #[arg(long, env = "EMBEDDING_TIMEOUT_MS", value_name = "MS")]
    pub embedding_timeout_ms: Option<String>,

    /// Retry attempts on HTTP 429 or 5xx from the embedder [default: 3].
    #[arg(long, env = "EMBEDDING_MAX_RETRIES", value_name = "N")]
    pub embedding_max_retries: Option<String>,

    /// Chunks longer than this are dropped rather than truncated [default: 8192].
    #[arg(long, env = "EMBEDDING_MAX_INPUT_TOKENS", value_name = "N")]
    pub embedding_max_input_tokens: Option<String>,

    // The three settings below were removed when the API, WebDAV and MCP
    // surfaces moved onto one listener. They are still accepted by the parser,
    // and hidden from `--help`, so that supplying one produces the startup
    // error naming its replacement (see `REMOVED_LISTENER_ENV_VARS`) instead of
    // clap's bare "unexpected argument", which would say nothing about what to
    // do next. Removing them from the parser entirely would make the flag form
    // less helpful than the variable form.
    /// Removed: WebDAV is served at /webdav on the main listener.
    #[arg(long, env = "NOTEDTHAT_WEBDAV_LISTEN_ADDR", hide = true)]
    pub webdav_listen_addr: Option<OsString>,

    /// Removed: MCP HTTP is served at /mcp on the main listener.
    #[arg(long, env = "NOTEDTHAT_MCP_HTTP_BIND", hide = true)]
    pub mcp_http_bind: Option<OsString>,

    /// Removed: MCP HTTP is always served at /mcp on the main listener.
    #[arg(long, env = "NOTEDTHAT_MCP_HTTP_ENABLED", hide = true)]
    pub mcp_http_enabled: Option<OsString>,
}

impl ServerCli {
    /// Read every setting from the environment alone, ignoring `argv`.
    ///
    /// # Errors
    ///
    /// Returns a `clap` error if a variable holds a value this parser cannot
    /// accept — in practice, non-UTF-8 in a setting typed as `String`.
    pub fn from_env() -> Result<Self, clap::Error> {
        Self::try_parse_from(["notedthat-server"])
    }
}

#[cfg(test)]
mod tests {
    use super::ServerCli;
    use clap::{CommandFactory as _, Parser as _};
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    /// Parse `args` with `vars` in the environment, as a real invocation would see
    /// both. The binary name is prepended, matching `argv`.
    fn parse(vars: &[(&str, Option<&str>)], args: &[&str]) -> ServerCli {
        let command_line: Vec<&str> = std::iter::once("notedthat-server")
            .chain(args.iter().copied())
            .collect();
        temp_env::with_vars(vars, || {
            ServerCli::try_parse_from(&command_line).expect("arguments must parse")
        })
    }

    #[test]
    fn a_flag_wins_over_the_variable_it_mirrors() {
        let cli = parse(
            &[("NOTEDTHAT_LISTEN_ADDR", Some("0.0.0.0:9999"))],
            &["--listen-addr", "127.0.0.1:8081"],
        );
        assert_eq!(cli.listen_addr.as_deref(), Some("127.0.0.1:8081"));
    }

    #[test]
    fn the_variable_is_used_when_no_flag_is_given() {
        let cli = parse(&[("NOTEDTHAT_LISTEN_ADDR", Some("0.0.0.0:9999"))], &[]);
        assert_eq!(cli.listen_addr.as_deref(), Some("0.0.0.0:9999"));
    }

    #[test]
    fn a_flag_alone_is_enough_with_the_environment_empty() {
        let cli = parse(
            &[("NOTEDTHAT_API_TOKEN", None)],
            &["--api-token", "flag-only"],
        );
        assert_eq!(cli.api_token.as_deref(), Some("flag-only"));
    }

    #[test]
    fn a_setting_neither_source_supplied_stays_absent() {
        let cli = parse(&[("NOTEDTHAT_LISTEN_ADDR", None)], &[]);
        assert!(cli.listen_addr.is_none());
    }

    /// Presence, not value: this is what makes `--s3-region ""` count as an
    /// offender in the cross-backend check, exactly as `NOTEDTHAT_S3_REGION=` does.
    #[test]
    fn an_empty_flag_value_is_still_a_supplied_value() {
        let cli = parse(&[("NOTEDTHAT_S3_REGION", None)], &["--s3-region", ""]);
        assert_eq!(cli.s3_region.as_deref(), Some(""));
    }

    /// A path is not obliged to be UTF-8, and neither source should be the place
    /// that loses one.
    #[test]
    fn a_path_setting_keeps_its_value_unmangled() {
        let cli = parse(
            &[("NOTEDTHAT_FS_ROOT", None)],
            &["--fs-root", "/srv/notedthat"],
        );
        assert_eq!(cli.fs_root, Some(OsString::from("/srv/notedthat")));
    }

    #[test]
    fn a_comma_separated_setting_arrives_whole_for_the_validator_to_split() {
        let cli = parse(&[("NOTEDTHAT_KBS", None)], &["--kbs", "notes,scratch"]);
        assert_eq!(cli.kbs.as_deref(), Some("notes,scratch"));
    }

    #[test]
    fn a_bare_boolean_flag_means_true() {
        let cli = parse(
            &[("NOTEDTHAT_S3_FORCE_PATH_STYLE", None)],
            &["--s3-force-path-style"],
        );
        assert_eq!(cli.s3_force_path_style.as_deref(), Some("true"));
    }

    /// The bare form must not become a one-way switch: a deployment that sets the
    /// variable to `true` has to be able to turn it off for one run.
    #[test]
    fn a_boolean_flag_can_still_be_given_false_explicitly() {
        let cli = parse(
            &[("NOTEDTHAT_S3_FORCE_PATH_STYLE", Some("true"))],
            &["--s3-force-path-style=false"],
        );
        assert_eq!(cli.s3_force_path_style.as_deref(), Some("false"));
    }

    /// Removed settings are still parsed — hidden from `--help`, but accepted — so
    /// that supplying one reaches the startup error naming its replacement rather
    /// than clap's bare "unexpected argument".
    #[test]
    fn a_removed_setting_is_accepted_by_the_parser_so_startup_can_explain_it() {
        let cli = parse(
            &[("NOTEDTHAT_MCP_HTTP_ENABLED", None)],
            &["--mcp-http-enabled", "false"],
        );
        assert_eq!(cli.mcp_http_enabled, Some(OsString::from("false")));
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        assert!(ServerCli::try_parse_from(["notedthat-server", "--nope"]).is_err());
    }

    /// Every setting the server reads is reachable from the command line.
    ///
    /// The list of variables lives in `config::tests::ALL_ENV_KEYS`, which already
    /// guards the storage adapters' own inventories; tying the parser to it means a
    /// setting added later without a flag fails the build rather than quietly
    /// staying environment-only.
    #[test]
    fn every_setting_has_both_a_flag_and_a_variable() {
        let command = ServerCli::command();
        let wired: BTreeSet<String> = command
            .get_arguments()
            .filter_map(|arg| Some(arg.get_env()?.to_string_lossy().into_owned()))
            .collect();

        let expected: BTreeSet<String> = crate::config::tests::ALL_ENV_KEYS
            .iter()
            .map(|name| (*name).to_string())
            .collect();

        assert_eq!(wired, expected);
    }

    /// The flag name is derived from the variable name by one mechanical rule, and
    /// `notedthat_core::flag_for` is what error messages use to name it. If a flag
    /// were spelled by hand differently, a diagnostic would point at a flag that
    /// does not exist.
    #[test]
    fn each_flag_is_spelled_the_way_diagnostics_will_name_it() {
        for arg in ServerCli::command().get_arguments() {
            let Some(env_var) = arg.get_env() else {
                continue;
            };
            let env_var = env_var.to_string_lossy();
            let expected = notedthat_core::flag_for(&env_var);
            let actual = format!(
                "--{}",
                arg.get_long().expect("every setting has a long flag")
            );
            assert_eq!(actual, expected, "{env_var} is wired to the wrong flag");
        }
    }

    /// Asking a running deployment for help must not print its credentials.
    #[test]
    fn help_never_echoes_a_credential_it_can_see_in_the_environment() {
        let vars = [
            ("NOTEDTHAT_API_TOKEN", Some("token-leak-canary")),
            ("NOTEDTHAT_WEBDAV_PASSWORD", Some("password-leak-canary")),
            ("NOTEDTHAT_S3_ACCESS_KEY_ID", Some("key-id-leak-canary")),
            ("NOTEDTHAT_S3_SECRET_ACCESS_KEY", Some("secret-leak-canary")),
            ("NOTEDTHAT_QDRANT_API_KEY", Some("qdrant-leak-canary")),
            ("EMBEDDING_API_KEY", Some("embedding-leak-canary")),
            // A non-credential, to show the value is hidden only where it must be.
            ("NOTEDTHAT_LISTEN_ADDR", Some("127.0.0.1:9999")),
        ];
        let help =
            temp_env::with_vars(vars, || ServerCli::command().render_long_help().to_string());

        assert!(!help.contains("leak-canary"), "{help}");
        assert!(help.contains("NOTEDTHAT_API_TOKEN"), "{help}");
        assert!(help.contains("127.0.0.1:9999"), "{help}");
    }

    #[test]
    fn the_help_text_warns_that_a_secret_on_the_command_line_is_visible() {
        let help = ServerCli::command().render_long_help().to_string();
        assert!(help.contains("ps"), "{help}");
        assert!(help.contains("--api-token"), "{help}");
    }
}
