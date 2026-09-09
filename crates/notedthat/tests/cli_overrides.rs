//! Subprocess tests for the command line of both shipped binaries.
//!
//! The unit tests prove the parser resolves a flag over a variable. These prove
//! it of the binaries an operator actually runs — that the wiring from `argv`
//! through to a startup diagnostic survives, and that `--help` is reachable.

use std::process::{Command, Stdio};

/// Resolved by Cargo at compile time: this suite lives in the package that
/// declares both binaries, so neither path needs runtime guessing.
const SERVER_BIN: &str = env!("CARGO_BIN_EXE_notedthat-server");
const MCP_STDIO_BIN: &str = env!("CARGO_BIN_EXE_notedthat-mcp-stdio");

/// A command with every setting cleared, so a variable that happens to be set on
/// the developer's machine or on CI cannot decide the outcome of a test.
fn clean(bin: &str) -> Command {
    let mut command = Command::new(bin);
    command.env_clear();
    command
}

/// Enough flags for the server to reach the listener; the storage root is
/// deliberately one that cannot be claimed, so startup fails after configuration
/// rather than serving. Only configuration errors are under test here.
fn minimal_server_args() -> Vec<&'static str> {
    vec![
        "--api-token",
        "flag-token",
        "--kbs",
        "notes",
        "--webdav-username",
        "dav",
        "--webdav-password",
        "dav-pass",
        "--qdrant-url",
        "http://127.0.0.1:6334",
        "--embedding-endpoint-url",
        "http://127.0.0.1:8088",
        "--embedding-model",
        "test-model",
        "--embedding-api-key",
        "test-key",
        "--embedding-dimensions",
        "1024",
    ]
}

#[test]
fn server_help_names_every_setting_both_ways_and_exits_zero() {
    let output = clean(SERVER_BIN).arg("--help").output().unwrap();
    assert!(output.status.success(), "--help must exit 0");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--api-token"), "{help}");
    assert!(help.contains("NOTEDTHAT_API_TOKEN"), "{help}");
    assert!(help.contains("--fs-root"), "{help}");
    assert!(help.contains("NOTEDTHAT_FS_ROOT"), "{help}");
}

#[test]
fn server_version_exits_zero() {
    let output = clean(SERVER_BIN).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")),
        "--version must print the crate version"
    );
}

/// The point of the whole feature: a flag decides, even when the variable is set
/// to something else.
///
/// The backend selector is the observable one, because its rejection quotes the
/// value it was given back verbatim — so the error itself says which source won.
/// An invalid listen address would not do: `SocketAddr`'s parse error names no
/// input, leaving nothing to tell the two apart.
#[test]
fn a_server_flag_overrides_the_variable_it_mirrors() {
    let output = clean(SERVER_BIN)
        .env("NOTEDTHAT_STORAGE_BACKEND", "from-the-variable")
        .args(minimal_server_args())
        .args(["--storage-backend", "from-the-flag"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("from-the-flag"),
        "the flag value must be the one parsed: {stderr}"
    );
    assert!(
        !stderr.contains("from-the-variable"),
        "the variable must not be the one parsed: {stderr}"
    );
}

/// The complement: with no flag, the variable still decides. This is what every
/// existing container deployment relies on.
#[test]
fn the_variable_still_decides_when_no_flag_is_given() {
    let output = clean(SERVER_BIN)
        .env("NOTEDTHAT_STORAGE_BACKEND", "from-the-variable")
        .args(minimal_server_args())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("from-the-variable"),
        "the variable must be the one parsed"
    );
}

/// A startup diagnostic has to be actionable whichever way the operator
/// configures the server, so it names the variable and the flag.
#[test]
fn a_startup_diagnostic_names_both_forms() {
    let output = clean(SERVER_BIN).output().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NOTEDTHAT_API_TOKEN"), "{stderr}");
    assert!(stderr.contains("--api-token"), "{stderr}");
}

/// A setting supplied only as a flag reaches the same cross-backend rule as the
/// variable form — the check is over resolved values, not over the environment.
#[test]
fn a_flag_supplied_setting_reaches_the_cross_backend_rule() {
    let output = clean(SERVER_BIN)
        .args(minimal_server_args())
        .args(["--fs-root", "/srv/notedthat"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NOTEDTHAT_STORAGE_BACKEND"), "{stderr}");
    assert!(stderr.contains("NOTEDTHAT_FS_ROOT"), "{stderr}");
}

/// The removed settings are hidden from `--help` but still parsed, so that
/// reaching for one as a flag gets the replacement rather than clap's bare
/// "unexpected argument".
#[test]
fn a_removed_setting_given_as_a_flag_names_its_replacement() {
    let output = clean(SERVER_BIN)
        .args(minimal_server_args())
        .args(["--mcp-http-enabled", "false"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("was removed"), "{stderr}");
    assert!(stderr.contains("/mcp"), "{stderr}");
}

#[test]
fn mcp_stdio_help_exits_zero_and_names_both_forms() {
    let output = clean(MCP_STDIO_BIN).arg("--help").output().unwrap();
    assert!(output.status.success());

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--url"), "{help}");
    assert!(help.contains("NOTEDTHAT_URL"), "{help}");
    assert!(help.contains("--token"), "{help}");
    assert!(help.contains("NOTEDTHAT_TOKEN"), "{help}");
}

/// The transport announces the URL it resolved before opening stdio, which is
/// what these two tests read.
///
/// Exit status cannot serve instead: with no client on the other end the service
/// loop ends on a closed connection and exits non-zero, whatever the
/// configuration was. Reaching the announcement at all is the signal that
/// configuration was accepted.
fn resolved_url_announcement(command: &mut Command) -> String {
    let output = command.stdin(Stdio::null()).output().unwrap();
    assert!(output.stdout.is_empty(), "stdout must stay clean");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Flags alone are enough: an MCP client that can pass arguments but not set
/// variables for a child process must still be able to launch this.
#[test]
fn mcp_stdio_starts_from_flags_with_no_variables_set() {
    let stderr = resolved_url_announcement(clean(MCP_STDIO_BIN).args([
        "--url",
        "http://127.0.0.1:8080",
        "--token",
        "flag-token",
    ]));

    assert!(
        stderr.contains("notedthat-mcp-stdio starting"),
        "flags alone must be a valid configuration: {stderr}"
    );
    assert!(stderr.contains("http://127.0.0.1:8080"), "{stderr}");
}

/// The variable here is a scheme the client rejects, so only the flag winning
/// can get the transport as far as announcing a URL.
#[test]
fn an_mcp_stdio_flag_overrides_the_variable_it_mirrors() {
    let stderr = resolved_url_announcement(
        clean(MCP_STDIO_BIN)
            .env("NOTEDTHAT_URL", "ftp://rejected.example.com")
            .args(["--url", "http://127.0.0.1:8080", "--token", "tok"]),
    );

    assert!(stderr.contains("http://127.0.0.1:8080"), "{stderr}");
    assert!(
        !stderr.contains("ftp://rejected.example.com"),
        "the variable must not be the one parsed: {stderr}"
    );
}

/// Stdout carries JSON-RPC and nothing else, so a parse error must not reach it.
#[test]
fn an_mcp_stdio_argument_error_leaves_stdout_empty() {
    let output = clean(MCP_STDIO_BIN).arg("--nope").output().unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "stdout must stay clean");
    assert!(!output.stderr.is_empty(), "the error belongs on stderr");
}
