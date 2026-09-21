//! `notedthat-mcp-stdio`: MCP-over-stdio transport for `NotedThat`.
//!
//! Takes the server URL and token from `--url` / `--token` or from
//! `NOTEDTHAT_URL` / `NOTEDTHAT_TOKEN` — the flag wins — validates them, and
//! serves MCP tools over stdio JSON-RPC.
//!
//! **Stdout is reserved for JSON-RPC.** All log output goes to stderr.
//!
//! The binary that drives [`run`] ships from the `notedthat` facade crate, so
//! that one published crate owns every installed binary name.
#![deny(missing_docs)]

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use notedthat_mcp::{NotedThatMcp, client::NotedThatClient};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

/// Command-line arguments, each mirroring one environment variable.
///
/// Both settings can be given either way and the flag wins, which is `clap`'s
/// precedence, granted by `env = "..."`. Neither is validated here: requiredness
/// and the empty-after-trim rule live in [`require`], so one check serves both
/// sources and its message keeps naming the variable an operator looks for.
// The doc comments below are rendered verbatim by `--help`.
#[allow(clippy::doc_markdown)]
#[derive(Parser, Debug, Default, Clone)]
#[command(
    name = "notedthat-mcp-stdio",
    version,
    about = "MCP-over-stdio transport for NotedThat",
    long_about = "MCP-over-stdio transport for NotedThat.\n\n\
                  Every setting can be given as the flag or as the environment variable \
                  beside it; the flag wins when both are set.\n\n\
                  --token is visible to any user on the host via `ps` and is recorded in \
                  shell history, so prefer NOTEDTHAT_TOKEN on a shared machine.\n\n\
                  Stdout carries JSON-RPC and nothing else; all logging goes to stderr."
)]
pub struct StdioCli {
    /// HTTP base URL of the running notedthat-server, e.g. http://localhost:8080.
    /// A trailing slash is stripped.
    #[arg(long, env = "NOTEDTHAT_URL", value_name = "URL")]
    pub url: Option<String>,

    /// Bearer token matching the server's NOTEDTHAT_API_TOKEN.
    #[arg(
        long,
        env = "NOTEDTHAT_TOKEN",
        value_name = "TOKEN",
        hide_env_values = true
    )]
    pub token: Option<String>,

    /// Most bytes one object read may fetch from the server; a larger object is
    /// refused with the read tool's slice arguments to use instead [default: 16777216].
    #[arg(long, env = "NOTEDTHAT_MCP_MAX_READ_BYTES", value_name = "BYTES")]
    pub mcp_max_read_bytes: Option<String>,
}

/// Serve MCP tools over stdio until the client disconnects.
///
/// Resolves the URL and token from the command line and the environment,
/// initializes stderr logging, validates them, then runs the stdio JSON-RPC
/// service loop.
///
/// Arguments are parsed before logging is initialized, so that `--help` and a
/// parse error reach the terminal without a subscriber having been installed.
///
/// # Errors
///
/// Returns an error if either setting is unsupplied or empty after trimming, if
/// they do not form a valid client configuration, or if the stdio transport or
/// service loop fails.
pub async fn run() -> Result<()> {
    let cli = StdioCli::parse();

    init_logging();

    let url = require(cli.url, "NOTEDTHAT_URL")?;
    let token = require(cli.token, "NOTEDTHAT_TOKEN")?;
    let max_read_bytes = read_budget(cli.mcp_max_read_bytes)?;

    let client = NotedThatClient::new(&url, &token)
        .context("invalid NOTEDTHAT_URL or NOTEDTHAT_TOKEN")?
        .with_max_read_bytes(max_read_bytes);

    tracing::info!(
        target: "notedthat_mcp_stdio",
        "notedthat-mcp-stdio starting; url = {}",
        client.base_url_display()
    );

    let service = NotedThatMcp::for_stdio(client)
        .serve(stdio())
        .await
        .context("stdio transport failed")?;

    service.waiting().await.context("service loop failed")?;

    Ok(())
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr) // CRITICAL: stdout reserved for JSON-RPC
        .with_ansi(false)
        .init();
}

/// Trim a supplied setting and reject it when nothing is left.
///
/// The diagnostic names both ways the setting can be given, because which one
/// the caller reached for is not knowable from here — and an MCP client launches
/// this binary from a config file, where the mistake is easy to make either way.
fn require(supplied: Option<String>, name: &str) -> Result<String> {
    let named = notedthat_core::setting(name);
    let Some(value) = supplied else {
        bail!("{named} is required but not set");
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{named} is required but is empty");
    }
    Ok(trimmed.to_string())
}

/// The read budget: absent, empty or blank means the default (Compose hands an
/// unset variable over as an empty string); anything else must be a positive
/// byte count.
fn read_budget(supplied: Option<String>) -> Result<u64> {
    let named = notedthat_core::setting("NOTEDTHAT_MCP_MAX_READ_BYTES");
    let Some(value) = supplied.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(notedthat_mcp::DEFAULT_MAX_READ_BYTES);
    };
    let budget: u64 = value
        .parse()
        .with_context(|| format!("{named} must be a valid u64 integer"))?;
    if budget == 0 {
        bail!("{named} must be > 0");
    }
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::{StdioCli, read_budget, require};
    use clap::Parser as _;

    fn parse(vars: &[(&str, Option<&str>)], args: &[&str]) -> StdioCli {
        let command_line: Vec<&str> = std::iter::once("notedthat-mcp-stdio")
            .chain(args.iter().copied())
            .collect();
        temp_env::with_vars(vars, || {
            StdioCli::try_parse_from(&command_line).expect("arguments must parse")
        })
    }

    #[test]
    fn a_flag_wins_over_the_variable_it_mirrors() {
        let cli = parse(
            &[("NOTEDTHAT_URL", Some("http://from-env:8080"))],
            &["--url", "http://from-flag:8080"],
        );
        assert_eq!(cli.url.as_deref(), Some("http://from-flag:8080"));
    }

    #[test]
    fn the_variable_is_used_when_no_flag_is_given() {
        let cli = parse(&[("NOTEDTHAT_TOKEN", Some("from-env"))], &[]);
        assert_eq!(cli.token.as_deref(), Some("from-env"));
    }

    #[test]
    fn an_unsupplied_setting_names_both_forms() {
        let error = require(None, "NOTEDTHAT_URL").unwrap_err().to_string();
        assert!(error.contains("NOTEDTHAT_URL"), "{error}");
        assert!(error.contains("--url"), "{error}");
    }

    #[test]
    fn a_setting_that_is_only_whitespace_is_refused() {
        let error = require(Some("   ".to_string()), "NOTEDTHAT_TOKEN")
            .unwrap_err()
            .to_string();
        assert!(error.contains("NOTEDTHAT_TOKEN"), "{error}");
        assert!(error.contains("is empty"), "{error}");
    }

    #[test]
    fn the_read_budget_defaults_and_refuses_zero_and_nonsense() {
        assert_eq!(
            read_budget(None).unwrap(),
            notedthat_mcp::DEFAULT_MAX_READ_BYTES
        );
        assert_eq!(read_budget(Some(" 4096 ".to_string())).unwrap(), 4096);
        for empty in ["", "  "] {
            assert_eq!(
                read_budget(Some(empty.to_string())).unwrap(),
                notedthat_mcp::DEFAULT_MAX_READ_BYTES,
                "{empty:?} is the default, as Compose's ${{VAR-}} requires"
            );
        }
        for (value, fragment) in [("0", "must be > 0"), ("lots", "must be a valid u64")] {
            let error = read_budget(Some(value.to_string()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("NOTEDTHAT_MCP_MAX_READ_BYTES"), "{error}");
            assert!(error.contains("--mcp-max-read-bytes"), "{error}");
            assert!(error.contains(fragment), "{error}");
        }
        let cli = parse(&[("NOTEDTHAT_MCP_MAX_READ_BYTES", Some("1024"))], &[]);
        assert_eq!(cli.mcp_max_read_bytes.as_deref(), Some("1024"));
    }

    #[test]
    fn a_supplied_setting_is_trimmed() {
        let value = require(
            Some("  http://localhost:8080  ".to_string()),
            "NOTEDTHAT_URL",
        )
        .expect("a value survives trimming");
        assert_eq!(value, "http://localhost:8080");
    }
}
