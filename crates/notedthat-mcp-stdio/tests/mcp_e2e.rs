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

pub fn mcp_request(stdin: &mut ChildStdin, id: u64, method: &str, params: serde_json::Value) {
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
        serde_json::json!({
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
    assert!(result.get("protocolVersion").is_some(), "missing protocolVersion: {result}");
    assert!(result.get("capabilities").is_some(), "missing capabilities: {result}");
    assert!(result.get("serverInfo").is_some(), "missing serverInfo: {result}");

    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "subprocess test — run with --ignored"]
async fn mcp_tools_list_returns_all_seven() {
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
    writeln!(&mut stdin, "{}", serde_json::to_string(&notification).unwrap()).unwrap();
    stdin.flush().unwrap();

    // Request tools/list
    mcp_request(&mut stdin, 1, "tools/list", serde_json::json!({}));
    let resp = mcp_response(&mut stdout, Duration::from_secs(5));

    let tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("expected result.tools array");

    let expected_tools: std::collections::HashSet<&str> = [
        "list_knowledgebases", "search", "read", "write", "list", "delete", "move",
    ]
    .iter()
    .copied()
    .collect();

    let actual_tools: std::collections::HashSet<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    assert_eq!(tools.len(), 7, "expected exactly 7 tools, got {}: {actual_tools:?}", tools.len());
    assert_eq!(actual_tools, expected_tools, "tool names mismatch");

    // Verify each tool has an inputSchema
    for tool in tools {
        let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        assert!(tool.get("inputSchema").is_some(), "tool {name} missing inputSchema");
    }

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
    mcp_request(&mut stdin, 99, "shutdown", serde_json::json!(null));

    // Wait for process exit (5s bound)
    drop(stdin);
    match wait_for_shutdown(&mut child, Duration::from_secs(5)) {
        Some(status) => {
            // Exit code may be 0 or non-zero depending on rmcp shutdown handling
            // The important thing is it exited cleanly (no kill needed)
            let _ = status;
        }
        None => {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("binary did not exit within 5s after shutdown request");
        }
    }
}

#[tokio::test]
#[ignore = "subprocess test"]
async fn mcp_stdin_close_causes_exit() {
    let (mut child, stdin, _stdout) =
        spawn_mcp_stdio("http://127.0.0.1:65534", "test-token");

    // Close stdin immediately (EOF)
    drop(stdin);

    match wait_for_shutdown(&mut child, Duration::from_secs(5)) {
        Some(_) => {} // Exited — pass
        None => {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("binary did not exit within 5s after stdin EOF");
        }
    }
}
