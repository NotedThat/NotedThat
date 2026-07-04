//! Subprocess tests verifying fail-fast env var validation.

use assert_cmd::Command;

fn binary() -> Command {
    Command::cargo_bin("notedthat-mcp-stdio").unwrap()
}

#[test]
fn missing_url_exits_nonzero() {
    let output = binary()
        .env_remove("NOTEDTHAT_URL")
        .env("NOTEDTHAT_TOKEN", "tok")
        .output()
        .unwrap();
    assert!(!output.status.success(), "should exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NOTEDTHAT_URL"), "stderr must mention var: {stderr}");
    assert!(output.stdout.is_empty(), "stdout must be empty");
}

#[test]
fn empty_url_exits_nonzero() {
    let output = binary()
        .env("NOTEDTHAT_URL", "")
        .env("NOTEDTHAT_TOKEN", "tok")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NOTEDTHAT_URL"), "stderr must mention var: {stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn whitespace_token_exits_nonzero() {
    let output = binary()
        .env("NOTEDTHAT_URL", "http://localhost:8080")
        .env("NOTEDTHAT_TOKEN", "   ")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NOTEDTHAT_TOKEN"), "stderr must mention var: {stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn missing_token_exits_nonzero() {
    let output = binary()
        .env("NOTEDTHAT_URL", "http://localhost:8080")
        .env_remove("NOTEDTHAT_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NOTEDTHAT_TOKEN"), "stderr: {stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn malformed_url_exits_nonzero() {
    let output = binary()
        .env("NOTEDTHAT_URL", "not-a-url")
        .env("NOTEDTHAT_TOKEN", "tok")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should mention either the var name or URL validity
    assert!(
        stderr.contains("NOTEDTHAT_URL") || stderr.contains("URL") || stderr.contains("url"),
        "stderr: {stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn ftp_scheme_exits_nonzero() {
    let output = binary()
        .env("NOTEDTHAT_URL", "ftp://example.com")
        .env("NOTEDTHAT_TOKEN", "tok")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
