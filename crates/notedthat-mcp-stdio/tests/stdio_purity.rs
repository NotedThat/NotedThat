//! Verify that stdout contains ONLY JSON-RPC messages (no log contamination).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const INITIALIZE_REQUEST: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

fn mcp_bin() -> std::path::PathBuf {
    // Use the binary built by Cargo for this test crate
    assert_cmd::cargo::cargo_bin("notedthat-mcp-stdio")
}

#[test]
fn stdout_is_pure_json_rpc() {
    // Use a URL that won't be connected to (binary doesn't need server for initialize)
    let mut child = Command::new(mcp_bin())
        .env("NOTEDTHAT_URL", "http://127.0.0.1:65534")
        .env("NOTEDTHAT_TOKEN", "test-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn binary");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Send initialize request
    writeln!(stdin, "{INITIALIZE_REQUEST}").unwrap();
    stdin.flush().unwrap();

    // Read response with timeout
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read line from stdout");

    // Verify it's valid JSON-RPC 2.0
    assert!(!line.trim().is_empty(), "stdout must not be empty after initialize");
    let json: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|_| panic!("stdout must be valid JSON, got: {line:?}"));
    assert_eq!(json.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"),
        "must be JSON-RPC 2.0: {json}");

    // Clean up
    drop(stdin);
    drop(reader);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn binary_exits_on_stdin_eof() {
    use std::time::Instant;

    let mut child = Command::new(mcp_bin())
        .env("NOTEDTHAT_URL", "http://127.0.0.1:65534")
        .env("NOTEDTHAT_TOKEN", "test-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn binary");

    // Close stdin immediately (EOF)
    drop(child.stdin.take());

    let start = Instant::now();
    let timeout = Duration::from_secs(5);

    loop {
        if let Some(status) = child.try_wait().unwrap() {
            // Exited — acceptable (any exit code on EOF is fine)
            let _ = status;
            return;
        }
        if start.elapsed() > timeout {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("binary did not exit within 5s after stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
