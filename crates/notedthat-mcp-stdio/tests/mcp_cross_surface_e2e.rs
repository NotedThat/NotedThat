#![allow(dead_code, missing_docs)]
// allow: SIZE_OK — task requires duplicating the container-backed MCP E2E fixture here.

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
const MCP_STDIO_BIN: &str = env!("CARGO_BIN_EXE_notedthat-mcp-stdio");

// SeaweedFS 4.18 requires an IAM config file to accept signed S3 requests.
const SEAWEEDFS_S3_CONFIG: &[u8] = br#"{"identities":[{"name":"test","credentials":[{"accessKey":"any","secretKey":"any"}],"actions":["Admin","Read","Write","List","Tagging"]}]}"#;

fn spawn_mcp_stdio(url: &str, token: &str) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(MCP_STDIO_BIN)
        .env("NOTEDTHAT_URL", url)
        .env("NOTEDTHAT_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn notedthat-mcp-stdio");
    let stdin = child.stdin.take().expect("child stdin pipe");
    let stdout = BufReader::new(child.stdout.take().expect("child stdout pipe"));
    (child, stdin, stdout)
}

fn mcp_request(stdin: &mut ChildStdin, id: u64, method: &str, params: serde_json::Value) {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&req).expect("serialize MCP request")
    )
    .expect("write MCP request");
    stdin.flush().expect("flush MCP request");
}

fn mcp_response(stdout: &mut BufReader<ChildStdout>, _timeout: Duration) -> serde_json::Value {
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

fn mcp_initialize(
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

fn mcp_session_init(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>) {
    let resp = mcp_initialize(stdin, stdout);
    assert!(resp.get("result").is_some(), "initialize failed: {resp}");

    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&notification).expect("serialize initialized notification")
    )
    .expect("write initialized notification");
    stdin.flush().expect("flush initialized notification");
}

fn mcp_call_tool(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    mcp_request(
        stdin,
        id,
        "tools/call",
        serde_json::json!({
            "name": name,
            "arguments": arguments,
        }),
    );
    mcp_response(stdout, Duration::from_secs(5))
}

fn wait_for_shutdown(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
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

fn unique_phrase(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    format!("{prefix}_{nanos}")
}

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
    kbs.insert(
        "notes".to_string(),
        KbSlug::try_new("notes").expect("valid KB slug"),
    );

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

/// Poll MCP search until a hit for `phrase` appears in KB `kb`, or timeout.
async fn poll_mcp_search(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    kb: &str,
    phrase: &str,
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();
    let mut id = 100_u64;
    while start.elapsed() < timeout {
        let resp = mcp_call_tool(
            stdin,
            stdout,
            id,
            "search",
            serde_json::json!({
                "kb": kb,
                "query": phrase,
                "limit": 5,
            }),
        );
        id += 1;

        if search_response_contains_phrase(&resp, phrase) {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn poll_mcp_search_gone(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    kb: &str,
    phrase: &str,
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();
    let mut id = 200_u64;
    while start.elapsed() < timeout {
        let resp = mcp_call_tool(
            stdin,
            stdout,
            id,
            "search",
            serde_json::json!({
                "kb": kb,
                "query": phrase,
                "limit": 5,
            }),
        );
        id += 1;

        if search_response_hit_count(&resp) == Some(0) {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

fn search_response_contains_phrase(resp: &serde_json::Value, phrase: &str) -> bool {
    let Some(content) = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };

    content.iter().any(|item| {
        item.get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains(phrase) || text_contains_hits(text))
    })
}

fn search_response_hit_count(resp: &serde_json::Value) -> Option<usize> {
    let content = resp.get("result")?.get("content")?.as_array()?;

    content.iter().find_map(|item| {
        let text = item.get("text")?.as_str()?;
        let v = serde_json::from_str::<serde_json::Value>(text).ok()?;
        v.get("hits")?.as_array().map(Vec::len)
    })
}

fn text_contains_hits(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("hits")
                .and_then(serde_json::Value::as_array)
                .map(|hits| !hits.is_empty())
        })
        .unwrap_or(false)
}

fn shutdown_child(mut child: Child, stdin: ChildStdin) {
    drop(stdin);
    if wait_for_shutdown(&mut child, Duration::from_secs(5)).is_none() {
        child.kill().expect("kill MCP child");
        child.wait().expect("wait for killed MCP child");
    }
}

#[tokio::test]
#[ignore = "requires SeaweedFS+Qdrant testcontainers"]
async fn mcp_write_becomes_searchable_via_mcp_search() {
    // Given: a fresh NotedThat server and MCP client with a unique markdown document.
    let fixture = start_notedthat_server_fixture().await;
    let unique_phrase = unique_phrase("UNIQUE_MCP_CROSS_SURFACE");
    let path = format!("cross-surface-{unique_phrase}.md");
    let content = format!("# Cross-surface test\n{unique_phrase}\nThis document is indexed.");
    let (child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, fixture.token);
    mcp_session_init(&mut stdin, &mut stdout);

    // When: MCP writes the document.
    let write_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "write",
        serde_json::json!({
            "kb": "notes",
            "path": path,
            "content": content,
            "mime_type": "text/markdown",
        }),
    );
    assert!(
        write_resp.get("error").is_none(),
        "write failed: {write_resp}"
    );

    // Then: MCP search returns that document after the indexer observes the write.
    let found = poll_mcp_search(
        &mut stdin,
        &mut stdout,
        "notes",
        &unique_phrase,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        found,
        "search did not return the written document within 10s"
    );

    shutdown_child(child, stdin);
}

#[tokio::test]
#[ignore = "requires SeaweedFS+Qdrant testcontainers"]
async fn mcp_delete_removes_from_search() {
    // Given: a fresh NotedThat server and a unique markdown document written via MCP.
    let fixture = start_notedthat_server_fixture().await;
    let unique_phrase = unique_phrase("UNIQUE_DELETE_TEST");
    let path = format!("delete-{unique_phrase}.md");
    let content = format!("# Delete test\n{unique_phrase}");
    let (child, mut stdin, mut stdout) = spawn_mcp_stdio(&fixture.http_url, fixture.token);
    mcp_session_init(&mut stdin, &mut stdout);

    let write_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "write",
        serde_json::json!({
            "kb": "notes",
            "path": path,
            "content": content,
            "mime_type": "text/markdown",
        }),
    );
    assert!(
        write_resp.get("error").is_none(),
        "write failed: {write_resp}"
    );

    let found = poll_mcp_search(
        &mut stdin,
        &mut stdout,
        "notes",
        &unique_phrase,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        found,
        "search did not return the written document within 10s"
    );

    // When: MCP deletes the document.
    let delete_resp = mcp_call_tool(
        &mut stdin,
        &mut stdout,
        2,
        "delete",
        serde_json::json!({
            "kb": "notes",
            "path": path,
        }),
    );
    assert!(
        delete_resp.get("error").is_none(),
        "delete failed: {delete_resp}"
    );

    // Then: MCP search stops returning hits for the unique phrase.
    let gone = poll_mcp_search_gone(
        &mut stdin,
        &mut stdout,
        "notes",
        &unique_phrase,
        Duration::from_secs(10),
    )
    .await;
    assert!(gone, "search still returned the deleted document after 10s");

    shutdown_child(child, stdin);
}
