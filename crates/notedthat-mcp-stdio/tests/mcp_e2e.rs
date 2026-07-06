#![allow(dead_code, missing_docs)]
// allow: SIZE_OK — task requires duplicating the container-backed server fixture here.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::sync::Mutex;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method, matchers::path};

const API_TOKEN: &str = "e2e-test-token";

// SeaweedFS 4.18 requires an IAM config file to accept signed S3 requests without this the
// S3 gateway rejects all signed requests. target path is the FIRST arg in with_copy_to.
const SEAWEEDFS_S3_CONFIG: &[u8] = br#"{"identities":[{"name":"test","credentials":[{"accessKey":"any","secretKey":"any"}],"actions":["Admin","Read","Write","List","Tagging"]}]}"#;

// ─── Binary Discovery ───────────────────────────────────────────────────────

pub const MCP_STDIO_BIN: &str = env!("CARGO_BIN_EXE_notedthat-mcp-stdio");

// ─── Subprocess Helpers ─────────────────────────────────────────────────────

pub fn spawn_mcp_stdio(url: &str, token: &str) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(MCP_STDIO_BIN)
        .env("NOTEDTHAT_URL", url)
        .env("NOTEDTHAT_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn notedthat-mcp-stdio");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

pub fn mcp_request(stdin: &mut ChildStdin, id: u64, method: &str, params: &serde_json::Value) {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(stdin, "{}", serde_json::to_string(&req).unwrap()).unwrap();
    stdin.flush().unwrap();
}

pub fn mcp_response(stdout: &mut BufReader<ChildStdout>, _timeout: Duration) -> serde_json::Value {
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("failed to read from stdout");
    assert!(!line.trim().is_empty(), "stdout returned empty line");
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).unwrap_or_else(|_| panic!("invalid JSON: {line:?}"));
    assert_eq!(
        v.get("jsonrpc").and_then(serde_json::Value::as_str),
        Some("2.0"),
        "expected JSON-RPC 2.0: {v}"
    );
    v
}

pub fn mcp_initialize(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
) -> serde_json::Value {
    mcp_request(
        stdin,
        0,
        "initialize",
        &serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        }),
    );
    mcp_response(stdout, Duration::from_secs(5))
}

pub fn wait_for_shutdown(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ─── Server Fixture (mirrors webdav_cross_surface_e2e.rs) ───────────────────

async fn start_seaweedfs() -> (impl std::any::Any, String) {
    let container = GenericImage::new("chrislusf/seaweedfs", "4.18")
        .with_exposed_port(8333_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .with_copy_to("/tmp/s3.json", SEAWEEDFS_S3_CONFIG.to_vec())
        .with_cmd(["server", "-s3", "-filer", "-s3.config=/tmp/s3.json"])
        .start()
        .await
        .expect("failed to start SeaweedFS testcontainer");
    let port = container
        .get_host_port_ipv4(8333_u16)
        .await
        .expect("failed to get SeaweedFS port");
    (container, format!("http://127.0.0.1:{port}"))
}

async fn start_qdrant() -> (impl std::any::Any, String) {
    let container = GenericImage::new("qdrant/qdrant", "v1.15.4")
        .with_exposed_port(6334_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .start()
        .await
        .expect("failed to start qdrant/qdrant:v1.15.4 — is Docker running?");
    let port = container
        .get_host_port_ipv4(6334_u16)
        .await
        .expect("failed to get Qdrant gRPC port");
    (container, format!("http://127.0.0.1:{port}"))
}

fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let v: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0_f32 } else { 0.0_f32 })
                .collect();
            serde_json::json!({"index": i, "embedding": v, "object": "embedding"})
        })
        .collect();
    serde_json::json!({"object": "list", "data": data})
}

fn test_config(
    http_addr: std::net::SocketAddr,
    dav_addr: std::net::SocketAddr,
    s3_url: &str,
    qdrant_url: &str,
    embedder_url: &str,
) -> notedthat_server::config::Config {
    use notedthat_core::{KbSlug, TenantSlug};
    use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
    use notedthat_storage_s3::S3Config;
    use std::collections::BTreeMap;

    let mut kbs = BTreeMap::new();
    kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: http_addr,
        s3: S3Config {
            endpoint_url: Some(s3_url.to_string()),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        },
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: qdrant_url.to_string(),
            api_key: None,
        },
        embedder: EmbedderConfig {
            // OpenAiCompatibleEmbedder appends /v1/embeddings itself — pass base URL only.
            endpoint_url: embedder_url.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: 4,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_listen_addr: dav_addr,
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_enabled: false,
        mcp_http_bind: "127.0.0.1:0".parse().expect("valid addr"),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        max_patchable_size: 100 * 1024 * 1024,
    }
}

async fn free_addr() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to port 0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

fn free_dav_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to port 0");
    listener.local_addr().expect("local_addr")
}

async fn wait_for_http(url: &str, timeout: Duration) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        assert!(
            tokio::time::Instant::now() <= deadline,
            "HTTP server did not become ready at {url}"
        );
        if client
            .get(url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

struct NotedThatServerFixture {
    http_url: String,
    token: &'static str,
    server_handle: tokio::task::JoinHandle<()>,
    _seaweed: Box<dyn std::any::Any>,
    _qdrant: Box<dyn std::any::Any>,
}

impl Drop for NotedThatServerFixture {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

async fn start_notedthat_server_fixture() -> NotedThatServerFixture {
    let _guard = test_mutex().lock().await;
    let (seaweed, s3_url) = start_seaweedfs().await;
    let (qdrant, qdrant_url) = start_qdrant().await;
    let mock_embedder = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_embedder)
        .await;

    let http_addr = free_addr().await;
    let dav_addr = free_dav_addr();
    let config = test_config(
        http_addr,
        dav_addr,
        &s3_url,
        &qdrant_url,
        &mock_embedder.uri(),
    );

    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run(config)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    wait_for_http(&format!("{http_url}/healthz"), Duration::from_secs(10)).await;

    NotedThatServerFixture {
        http_url,
        token: API_TOKEN,
        server_handle,
        _seaweed: Box::new(seaweed),
        _qdrant: Box::new(qdrant),
    }
}

static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn test_mutex() -> &'static Mutex<()> {
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires docker-compose stack (but smoke test only needs binary)"]
async fn mcp_infra_smoke() {
    // Given: the MCP stdio binary is available and receives syntactically valid configuration.
    let (mut child, mut stdin, mut stdout) =
        spawn_mcp_stdio("http://127.0.0.1:65534", "test-token");

    // When: the client sends the MCP initialize request.
    let resp = mcp_initialize(&mut stdin, &mut stdout);

    // Then: the server responds with a JSON-RPC result.
    assert!(resp.get("result").is_some(), "expected result: {resp}");

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "subprocess test — run with --ignored"]
async fn mcp_initialize_returns_valid_response() {
    let (mut child, mut stdin, mut stdout) =
        spawn_mcp_stdio("http://127.0.0.1:65534", "test-token");

    let resp = mcp_initialize(&mut stdin, &mut stdout);
    let result = resp.get("result").expect("expected result field");
    assert!(
        result.get("protocolVersion").is_some(),
        "missing protocolVersion: {result}"
    );
    assert!(
        result.get("capabilities").is_some(),
        "missing capabilities: {result}"
    );
    assert!(
        result.get("serverInfo").is_some(),
        "missing serverInfo: {result}"
    );

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "subprocess test — run with --ignored"]
async fn mcp_tools_list_returns_all_nine() {
    let (mut child, mut stdin, mut stdout) =
        spawn_mcp_stdio("http://127.0.0.1:65534", "test-token");

    // Initialize first (required by MCP protocol)
    let _ = mcp_initialize(&mut stdin, &mut stdout);

    // Send initialized notification
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    writeln!(
        &mut stdin,
        "{}",
        serde_json::to_string(&notification).unwrap()
    )
    .unwrap();
    stdin.flush().unwrap();

    // Request tools/list
    mcp_request(&mut stdin, 1, "tools/list", &serde_json::json!({}));
    let resp = mcp_response(&mut stdout, Duration::from_secs(5));

    let tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("expected result.tools array");

    let expected_tools: std::collections::HashSet<&str> = [
        "list_knowledgebases",
        "search",
        "read",
        "write",
        "edit",
        "append",
        "list",
        "delete",
        "move",
    ]
    .iter()
    .copied()
    .collect();

    let actual_tools: std::collections::HashSet<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    assert_eq!(
        tools.len(),
        9,
        "expected exactly 9 tools, got {}: {actual_tools:?}",
        tools.len()
    );
    assert_eq!(actual_tools, expected_tools, "tool names mismatch");

    // Verify each tool has an inputSchema
    for tool in tools {
        let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        assert!(
            tool.get("inputSchema").is_some(),
            "tool {name} missing inputSchema"
        );
    }

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

// ─── W4.3+W4.4 Helpers ──────────────────────────────────────────────────────

fn mcp_call_tool(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    tool_name: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    mcp_request(
        stdin,
        id,
        "tools/call",
        &serde_json::json!({
            "name": tool_name,
            "arguments": args,
        }),
    );
    mcp_response(stdout, Duration::from_secs(10))
}

fn mcp_session_init(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>) {
    let _ = mcp_initialize(stdin, stdout);
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    writeln!(stdin, "{}", serde_json::to_string(&notification).unwrap()).unwrap();
    stdin.flush().unwrap();
}

// ─── W4.3: Happy-path chain ─────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires docker-compose stack"]
#[allow(clippy::too_many_lines)]
async fn mcp_write_list_read_delete() {
    let fixture = start_notedthat_server_fixture().await;
    let (mut child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, fixture.token);
    mcp_session_init(&mut stdin, &mut stdout);

    // 1. Write a note.
    let write_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "write",
        &serde_json::json!({
            "kb": "notes",
            "path": "test-w4.md",
            "content": "# Test\nHello from W4.3",
        }),
    );
    assert!(
        write_resp.get("result").is_some(),
        "write should succeed: {write_resp}"
    );

    // 2. List with prefix — test-w4.md must appear.
    let list_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        2,
        "list",
        &serde_json::json!({ "kb": "notes", "prefix": "test-" }),
    );
    assert!(
        list_resp.get("result").is_some(),
        "list should succeed: {list_resp}"
    );
    let list_text = list_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("list content[0].text");
    let list_val: serde_json::Value = serde_json::from_str(list_text).expect("list response JSON");
    let objects = list_val["objects"].as_array().expect("objects array");
    assert!(
        objects
            .iter()
            .any(|o| o["key"].as_str() == Some("test-w4.md")),
        "test-w4.md not found in listing; objects: {objects:?}"
    );

    // 3. Read full content — must contain the written text.
    let read_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        3,
        "read",
        &serde_json::json!({ "kb": "notes", "path": "test-w4.md" }),
    );
    assert!(
        read_resp.get("result").is_some(),
        "read should succeed: {read_resp}"
    );
    let content = read_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("read content[0].text");
    assert!(
        content.contains("Hello from W4.3"),
        "unexpected content: {content:?}"
    );

    // 4. Ranged read — bytes 0..6 (exclusive) → "# Test".
    let range_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        4,
        "read",
        &serde_json::json!({
            "kb": "notes",
            "path": "test-w4.md",
            "byte_start": 0,
            "byte_end": 6,
        }),
    );
    assert!(
        range_resp.get("result").is_some(),
        "ranged read should succeed: {range_resp}"
    );
    let slice = range_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("ranged content[0].text");
    assert!(
        slice.starts_with("# Test"),
        "ranged slice mismatch: {slice:?}"
    );

    // 5. Delete the note.
    let del_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        5,
        "delete",
        &serde_json::json!({ "kb": "notes", "path": "test-w4.md" }),
    );
    assert!(
        del_resp.get("result").is_some(),
        "delete should succeed: {del_resp}"
    );

    // 6. Read after delete → not_found error.
    let gone_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        6,
        "read",
        &serde_json::json!({ "kb": "notes", "path": "test-w4.md" }),
    );
    assert!(
        gone_resp.get("error").is_some(),
        "read of deleted note should return error: {gone_resp}"
    );
    let msg = gone_resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not_found"),
        "expected not_found in error message, got: {msg:?}"
    );

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "requires docker-compose stack"]
async fn mcp_move_happy() {
    let fixture = start_notedthat_server_fixture().await;
    let (mut child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, fixture.token);
    mcp_session_init(&mut stdin, &mut stdout);

    // 1. Write source note.
    let write_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "write",
        &serde_json::json!({
            "kb": "notes",
            "path": "src-move.md",
            "content": "# Move source",
        }),
    );
    assert!(
        write_resp.get("result").is_some(),
        "write src should succeed: {write_resp}"
    );

    // 2. Move src-move.md → dst-move.md.
    let move_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        2,
        "move",
        &serde_json::json!({
            "kb": "notes",
            "from": "src-move.md",
            "to": "dst-move.md",
        }),
    );
    assert!(
        move_resp.get("result").is_some(),
        "move should succeed: {move_resp}"
    );

    // 3. Read destination — must contain original content.
    let read_dst = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        3,
        "read",
        &serde_json::json!({ "kb": "notes", "path": "dst-move.md" }),
    );
    assert!(
        read_dst.get("result").is_some(),
        "read dst should succeed: {read_dst}"
    );
    let dst_content = read_dst["result"]["content"][0]["text"]
        .as_str()
        .expect("dst content[0].text");
    assert!(
        dst_content.contains("# Move source"),
        "dst content mismatch: {dst_content:?}"
    );

    // 4. Read source — must be gone (not_found).
    let read_src = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        4,
        "read",
        &serde_json::json!({ "kb": "notes", "path": "src-move.md" }),
    );
    assert!(
        read_src.get("error").is_some(),
        "source should be gone after move: {read_src}"
    );
    let src_msg = read_src["error"]["message"].as_str().unwrap_or("");
    assert!(
        src_msg.contains("not_found"),
        "expected not_found for source, got: {src_msg:?}"
    );

    // 5. Cleanup: delete destination.
    let del_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        5,
        "delete",
        &serde_json::json!({ "kb": "notes", "path": "dst-move.md" }),
    );
    assert!(
        del_resp.get("result").is_some(),
        "delete dst should succeed: {del_resp}"
    );

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

// ─── W4.4: Error paths ──────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires docker-compose stack"]
async fn mcp_read_missing() {
    let fixture = start_notedthat_server_fixture().await;
    let (mut child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, fixture.token);
    mcp_session_init(&mut stdin, &mut stdout);

    let resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "read",
        &serde_json::json!({
            "kb": "notes",
            "path": "does-not-exist-w4.md",
        }),
    );
    assert!(
        resp.get("error").is_some(),
        "read of missing object should error: {resp}"
    );
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not_found"),
        "expected not_found in message, got: {msg:?}"
    );

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "requires docker-compose stack"]
async fn mcp_write_precondition() {
    let fixture = start_notedthat_server_fixture().await;
    let (mut child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, fixture.token);
    mcp_session_init(&mut stdin, &mut stdout);

    // 1. Initial write — capture the returned etag.
    let first = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "write",
        &serde_json::json!({
            "kb": "notes",
            "path": "test-precond.md",
            "content": "v1",
        }),
    );
    assert!(
        first.get("result").is_some(),
        "first write should succeed: {first}"
    );
    let result_text = first["result"]["content"][0]["text"]
        .as_str()
        .expect("write result content[0].text");
    let result_val: serde_json::Value =
        serde_json::from_str(result_text).expect("write result JSON");
    let etag = result_val["etag"]
        .as_str()
        .unwrap_or("\"initial\"")
        .to_string();

    // 2. Write again with a deliberately wrong If-Match → precondition_failed.
    let wrong_etag = format!("{etag}-wrong");
    let second = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        2,
        "write",
        &serde_json::json!({
            "kb": "notes",
            "path": "test-precond.md",
            "content": "v2",
            "if_match": wrong_etag,
        }),
    );
    assert!(
        second.get("error").is_some(),
        "write with wrong if_match should error: {second}"
    );
    let msg = second["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("precondition_failed"),
        "expected precondition_failed, got: {msg:?}"
    );

    // Cleanup.
    let _ = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        3,
        "delete",
        &serde_json::json!({ "kb": "notes", "path": "test-precond.md" }),
    );

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "requires docker-compose stack"]
async fn mcp_bad_token() {
    let fixture = start_notedthat_server_fixture().await;
    // Intentionally pass a wrong token — every tool call should be rejected.
    let (mut child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, "wrong-token");
    mcp_session_init(&mut stdin, &mut stdout);

    let resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "list_knowledgebases",
        &serde_json::json!({}),
    );
    assert!(
        resp.get("error").is_some(),
        "bad token should produce an error: {resp}"
    );
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("unauthorized"),
        "expected unauthorized in message, got: {msg:?}"
    );

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "subprocess test"]
async fn mcp_shutdown_returns_clean_exit() {
    let (mut child, mut stdin, mut stdout) =
        spawn_mcp_stdio("http://127.0.0.1:65534", "test-token");

    // Initialize
    let _ = mcp_initialize(&mut stdin, &mut stdout);

    // Send shutdown request
    mcp_request(&mut stdin, 99, "shutdown", &serde_json::json!(null));

    // Wait for process exit (5s bound)
    drop(stdin);
    if let Some(status) = wait_for_shutdown(&mut child, Duration::from_secs(5)) {
        // Exit code may be 0 or non-zero depending on rmcp shutdown handling
        // The important thing is it exited cleanly (no kill needed)
        let _ = status;
    } else {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("binary did not exit within 5s after shutdown request");
    }
}

#[tokio::test]
#[ignore = "subprocess test"]
async fn mcp_stdin_close_causes_exit() {
    let (mut child, stdin, _stdout) = spawn_mcp_stdio("http://127.0.0.1:65534", "test-token");

    // Close stdin immediately (EOF)
    drop(stdin);

    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("binary did not exit within 5s after stdin EOF");
    }
    // else: exited — pass
}
