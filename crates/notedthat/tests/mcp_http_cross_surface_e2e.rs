#![allow(missing_docs)]

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// How long to wait for the server to bind after startup begins.
///
/// Provisioning is in-process now, so this is generous by a wide margin; it
/// exists to fail with a clear message rather than hang if startup breaks.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);

const API_TOKEN: &str = "e2e-test-token";
const EXPECTED_M7_TOOLS: &str =
    "list_knowledgebases,search,read,write,list,delete,move,append,edit,replace";
const MCP_STDIO_BIN: &str = env!("CARGO_BIN_EXE_notedthat-mcp-stdio");

/// Vector width the stub embedder and the provisioned collection agree on.
const EMBEDDING_DIM: u32 = 4;

/// Storage, vector store and embedder, all in-process.
///
/// The subject of these tests is the server's cross-surface behaviour, not any
/// backend's wire protocol, so the whole runtime is assembled here and handed to
/// `run_with`. Startup, provisioning, the indexer worker and every listener are
/// still the real path.
fn in_memory_backends() -> notedthat_server::run::Backends {
    notedthat_server::run::Backends {
        storage: std::sync::Arc::new(notedthat_api_http::testing::InMemoryStorage::default()),
        store: std::sync::Arc::new(notedthat_indexer::testing::InMemoryVectorStore::new()),
        embedder: std::sync::Arc::new(notedthat_indexer::testing::StubEmbedder::new(
            EMBEDDING_DIM as usize,
        )),
    }
}

fn test_config_with_mcp_http(
    listen_addr: std::net::SocketAddr,
) -> notedthat_server::config::Config {
    use notedthat_core::{KbSlug, TenantSlug};
    use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
    use std::collections::BTreeMap;

    let mut kbs = BTreeMap::new();
    kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            // OpenAiCompatibleEmbedder appends /v1/embeddings itself — pass base URL only.
            endpoint_url: "http://127.0.0.1:1".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: EMBEDDING_DIM,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        max_patchable_size: 10 * 1024 * 1024,
        staging: notedthat_core::StagingConfig::default(),
    }
}

fn test_config_with_kbs_and_mcp_http(
    kbs: &[&str],
    listen_addr: std::net::SocketAddr,
) -> notedthat_server::config::Config {
    use notedthat_core::{KbSlug, TenantSlug};
    use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
    use std::collections::BTreeMap;

    let mut kb_map = BTreeMap::new();
    for kb in kbs {
        kb_map.insert((*kb).to_string(), KbSlug::try_new(*kb).unwrap());
    }

    Config {
        api_token: API_TOKEN.to_string(),
        kbs: kb_map,
        tenant_slug: TenantSlug::default(),
        listen_addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:1".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: EMBEDDING_DIM,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        max_patchable_size: 10 * 1024 * 1024,
        staging: notedthat_core::StagingConfig::default(),
    }
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
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn mcp_request(
    client: &reqwest::Client,
    mcp_url: &str,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let response = client
        .post(mcp_url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("MCP HTTP request failed");
    assert!(
        response.status().is_success(),
        "MCP HTTP {method} should succeed, got {}",
        response.status()
    );
    response
        .json::<serde_json::Value>()
        .await
        .expect("MCP HTTP response must be JSON")
}

async fn mcp_call_tool(
    client: &reqwest::Client,
    mcp_url: &str,
    id: u64,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    mcp_request(
        client,
        mcp_url,
        id,
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
    .await
}

fn mcp_json_content(response: &serde_json::Value) -> serde_json::Value {
    if !response["result"]["structuredContent"].is_null() {
        return response["result"]["structuredContent"].clone();
    }

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("MCP tool result must contain JSON content: {response}"));
    serde_json::from_str(text).expect("MCP JSON content must parse")
}

fn search_hit_identity(hit: &serde_json::Value) -> (String, u64, u64) {
    let object_key = hit["object_key"]
        .as_str()
        .expect("search hit should include object_key")
        .to_string();
    let byte_start = hit["byte_start"]
        .as_u64()
        .expect("search hit should include byte_start");
    let byte_end = hit["byte_end"]
        .as_u64()
        .expect("search hit should include byte_end");
    (object_key, byte_start, byte_end)
}

fn spawn_mcp_stdio(url: &str, token: &str) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(MCP_STDIO_BIN)
        .env("NOTEDTHAT_URL", url)
        .env("NOTEDTHAT_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn notedthat-mcp-stdio");
    let stdin = child
        .stdin
        .take()
        .expect("stdio child stdin should be piped");
    let stdout = BufReader::new(
        child
            .stdout
            .take()
            .expect("stdio child stdout should be piped"),
    );
    (child, stdin, stdout)
}

async fn stdio_mcp_request(
    stdin: &mut ChildStdin,
    id: u64,
    method: &str,
    params: serde_json::Value,
) {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&request).unwrap();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("failed to write stdio MCP request");
    stdin
        .flush()
        .await
        .expect("failed to flush stdio MCP stdin");
}

async fn stdio_mcp_initialized(stdin: &mut ChildStdin) {
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    let mut line = serde_json::to_string(&notification).unwrap();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .expect("failed to write stdio MCP initialized notification");
    stdin
        .flush()
        .await
        .expect("failed to flush stdio MCP stdin");
}

async fn stdio_mcp_response(stdout: &mut BufReader<ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line))
        .await
        .expect("timed out reading stdio MCP stdout")
        .expect("failed to read from stdio MCP stdout");
    assert!(
        !line.trim().is_empty(),
        "stdio MCP stdout returned an empty line"
    );
    let response: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|_| panic!("stdio MCP response must be JSON: {line:?}"));
    assert_eq!(
        response.get("jsonrpc").and_then(serde_json::Value::as_str),
        Some("2.0"),
        "stdio MCP response must be JSON-RPC 2.0: {response}"
    );
    response
}

async fn stdio_mcp_call_tool(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    stdio_mcp_request(
        stdin,
        id,
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
    .await;
    stdio_mcp_response(stdout).await
}

async fn stop_stdio_child(child: &mut Child, stdin: ChildStdin) {
    drop(stdin);
    if child
        .try_wait()
        .expect("failed to poll stdio child")
        .is_none()
    {
        child.start_kill().expect("failed to kill stdio child");
        child.wait().await.expect("failed to wait for stdio child");
    }
}

async fn poll_mcp_search_hit(
    client: &reqwest::Client,
    mcp_url: &str,
    phrase: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut id = 10_u64;
    loop {
        if tokio::time::Instant::now() > deadline {
            return None;
        }

        let response = mcp_call_tool(
            client,
            mcp_url,
            id,
            "search",
            serde_json::json!({
                "kb": "notes",
                "query": phrase,
                "limit": 5,
            }),
        )
        .await;
        id += 1;

        if response.get("error").is_none() {
            let search_result = mcp_json_content(&response);
            if let Some(hit) = search_result["hits"].as_array().and_then(|hits| {
                hits.iter()
                    .find(|hit| hit["object_key"].as_str() == Some("e2e.md"))
            }) {
                return Some(hit.clone());
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn mcp_http_initialize_tools() {
    // Given: all three server listeners are configured on random loopback ports.
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_mcp_http(http_addr);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();

    // When: an authenticated MCP Streamable HTTP client initializes and lists tools.
    let initialize = mcp_request(
        &client,
        &mcp_url,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-e2e", "version": "0" }
        }),
    )
    .await;

    // Then: initialize advertises Resources as an empty capability object.
    assert_eq!(
        initialize
            .get("jsonrpc")
            .and_then(serde_json::Value::as_str),
        Some("2.0"),
        "initialize response must be JSON-RPC 2.0: {initialize}"
    );
    let capabilities = &initialize["result"]["capabilities"];
    let resources = capabilities
        .get("resources")
        .expect("initialize must advertise resources capability");
    assert!(
        resources.is_object(),
        "resources capability must be an object: {resources}"
    );
    assert_eq!(
        resources.as_object().map(serde_json::Map::len),
        Some(0),
        "M8 resources capability should be an empty object"
    );

    let tools_list = mcp_request(&client, &mcp_url, 1, "tools/list", serde_json::json!({})).await;
    let tools = tools_list["result"]["tools"]
        .as_array()
        .expect("tools/list result must contain tools array");
    assert_eq!(
        tools.len(),
        EXPECTED_M7_TOOLS.split(',').count(),
        "tools/list response: {tools_list}"
    );

    for expected_tool in EXPECTED_M7_TOOLS.split(',') {
        assert!(
            tools
                .iter()
                .any(|tool| tool.get("name").and_then(serde_json::Value::as_str)
                    == Some(expected_tool)),
            "tools/list must include {expected_tool:?}: {tools_list}"
        );
    }

    server_handle.abort();
}

#[tokio::test]
async fn mcp_http_write_search_identity() {
    // Given: MCP HTTP, the HTTP API, and the in-process backends are running.
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_mcp_http(http_addr);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let phrase = format!("notedthat_m8_http_write_search_unique_{nonce}");
    let content = format!("# MCP HTTP identity\n\n{phrase}\n");

    let initialize = mcp_request(
        &client,
        &mcp_url,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-e2e", "version": "0" }
        }),
    )
    .await;
    assert!(
        initialize.get("result").is_some(),
        "initialize should succeed before tools/call: {initialize}"
    );

    // When: a note is written via MCP HTTP and queried via the MCP HTTP search tool.
    let write_response = mcp_call_tool(
        &client,
        &mcp_url,
        1,
        "write",
        serde_json::json!({
            "kb": "notes",
            "path": "e2e.md",
            "content": content,
            "mime_type": "text/markdown",
        }),
    )
    .await;
    assert!(
        write_response.get("result").is_some(),
        "MCP HTTP write should succeed: {write_response}"
    );

    let found_hit = poll_mcp_search_hit(&client, &mcp_url, &phrase, Duration::from_secs(40)).await;
    let hit = found_hit.expect("MCP HTTP search should return the written phrase within 40 s");

    // Then: the returned identity is the exact object and a valid byte coordinate range.
    assert_eq!(
        hit["object_key"].as_str(),
        Some("e2e.md"),
        "search hit should identify e2e.md: {hit}"
    );
    let byte_start = hit["byte_start"]
        .as_u64()
        .expect("search hit should include byte_start");
    let byte_end = hit["byte_end"]
        .as_u64()
        .expect("search hit should include byte_end");
    assert!(
        byte_start < byte_end,
        "search hit byte range should be non-empty: {hit}"
    );

    server_handle.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_http_write_stdio_search_identity() {
    // Given: MCP HTTP, the HTTP API, and the in-process backends are running.
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_mcp_http(http_addr);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let phrase = format!("notedthat_m8_http_write_stdio_search_unique_{nonce}");
    let content = format!("# MCP HTTP to stdio identity\n\n{phrase}\n");

    let initialize = mcp_request(
        &client,
        &mcp_url,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-e2e", "version": "0" }
        }),
    )
    .await;
    assert!(
        initialize.get("result").is_some(),
        "initialize should succeed before tools/call: {initialize}"
    );

    let write_response = mcp_call_tool(
        &client,
        &mcp_url,
        1,
        "write",
        serde_json::json!({
            "kb": "notes",
            "path": "e2e.md",
            "content": content,
            "mime_type": "text/markdown",
        }),
    )
    .await;
    assert!(
        write_response.get("result").is_some(),
        "MCP HTTP write should succeed: {write_response}"
    );

    // When: HTTP search confirms indexing, then stdio search queries the same HTTP API.
    let found_hit = poll_mcp_search_hit(&client, &mcp_url, &phrase, Duration::from_secs(40)).await;
    let http_hit = found_hit.expect("MCP HTTP search should return the written phrase within 40 s");
    let http_identity = search_hit_identity(&http_hit);

    let (mut child, mut stdin, mut stdout) = spawn_mcp_stdio(&http_url, API_TOKEN);
    stdio_mcp_request(
        &mut stdin,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-e2e-stdio", "version": "0" }
        }),
    )
    .await;
    let stdio_initialize = stdio_mcp_response(&mut stdout).await;
    assert!(
        stdio_initialize.get("result").is_some(),
        "stdio initialize should succeed: {stdio_initialize}"
    );
    stdio_mcp_initialized(&mut stdin).await;

    let stdio_search = stdio_mcp_call_tool(
        &mut stdin,
        &mut stdout,
        1,
        "search",
        serde_json::json!({
            "kb": "notes",
            "query": phrase,
            "limit": 5,
        }),
    )
    .await;
    let stdio_result = mcp_json_content(&stdio_search);
    let stdio_hit = stdio_result["hits"]
        .as_array()
        .and_then(|hits| {
            hits.iter()
                .find(|hit| hit["object_key"].as_str() == Some("e2e.md"))
        })
        .unwrap_or_else(|| panic!("stdio search should return e2e.md hit: {stdio_result}"));
    let stdio_identity = search_hit_identity(stdio_hit);

    // Then: both MCP surfaces report the same object and byte coordinate identity.
    assert_eq!(
        stdio_identity, http_identity,
        "stdio search identity should exactly match MCP HTTP search identity; http_hit={http_hit}, stdio_hit={stdio_hit}"
    );

    stop_stdio_child(&mut child, stdin).await;
    server_handle.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn unified_http_auth_matrix_and_legacy_mcp_refusal() {
    const EXACT_SSE_REFUSAL_BODY: &str = r#"{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}"#;

    // Given: every HTTP surface is mounted on one random loopback listener.
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_mcp_http(http_addr);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    let sse_url = format!("http://{http_addr}/sse");
    let api_url = format!("http://{http_addr}/api/v1/knowledgebases");
    let old_api_url = format!("http://{http_addr}/v1/knowledgebases");
    let webdav_url = format!("http://{http_addr}/webdav");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    // Raw reqwest client — no MCP library, no redirect following.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client");

    let resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("POST /mcp (no auth) failed");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "POST /mcp without Authorization header must return 401"
    );

    let resp = client
        .post(&mcp_url)
        .header("Authorization", "Bearer wrongtoken")
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("POST /mcp (wrong bearer) failed");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "POST /mcp with wrong Bearer token must return 401"
    );

    let resp = client
        .get(&api_url)
        .basic_auth("e2e-webdav-user", Some("e2e-webdav-pass"))
        .send()
        .await
        .expect("GET API with Basic credentials failed");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "WebDAV Basic credentials must not authorize API requests"
    );

    let resp = client
        .post(&mcp_url)
        .basic_auth("e2e-webdav-user", Some("e2e-webdav-pass"))
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("POST MCP with Basic credentials failed");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "WebDAV Basic credentials must not authorize MCP requests"
    );

    let resp = client
        .request(reqwest::Method::OPTIONS, &webdav_url)
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .expect("OPTIONS WebDAV with Bearer credentials failed");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "API Bearer credentials must not authorize WebDAV requests"
    );

    let resp = client
        .request(reqwest::Method::OPTIONS, &webdav_url)
        .basic_auth("e2e-webdav-user", Some("e2e-webdav-pass"))
        .send()
        .await
        .expect("OPTIONS WebDAV with Basic credentials failed");
    assert_eq!(
        resp.status().as_u16(),
        204,
        "valid Basic credentials must reach WebDAV on the shared listener"
    );

    let resp = client
        .get(&old_api_url)
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .expect("GET removed /v1 route failed");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "the removed /v1 API prefix must not redirect or alias"
    );

    //    SSE refusal fires before auth, so no Authorization header needed.
    let resp = client
        .post(&sse_url)
        .header("Content-Type", "application/json")
        .body(r"{}")
        .send()
        .await
        .expect("POST /sse failed");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("failed to read POST /sse body");
    assert_eq!(status, 405, "POST /sse must return 405; body: {body:?}");
    assert_eq!(
        body, EXACT_SSE_REFUSAL_BODY,
        "POST /sse body must be exact SSE refusal JSON"
    );

    let resp = client.get(&mcp_url).send().await.expect("GET /mcp failed");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("failed to read GET /mcp body");
    assert_eq!(status, 405, "GET /mcp must return 405; body: {body:?}");
    assert_eq!(
        body, EXACT_SSE_REFUSAL_BODY,
        "GET /mcp body must be exact SSE refusal JSON"
    );

    let resp = client
        .delete(&mcp_url)
        .send()
        .await
        .expect("DELETE /mcp failed");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("failed to read DELETE /mcp body");
    assert_eq!(status, 405, "DELETE /mcp must return 405; body: {body:?}");
    assert_eq!(
        body, EXACT_SSE_REFUSAL_BODY,
        "DELETE /mcp body must be exact SSE refusal JSON"
    );

    //    Uses valid auth so the request reaches rmcp's Origin-validation layer.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Origin", "https://evil.example.com")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("POST /mcp (hostile origin) failed");
    assert_eq!(
        resp.status().as_u16(),
        403,
        "POST /mcp with hostile Origin must return 403"
    );

    //    Overrides the Host header so rmcp's allowed_hosts check rejects the request.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header(reqwest::header::HOST, "attacker.example.com")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("POST /mcp (disallowed host) failed");
    assert_eq!(
        resp.status().as_u16(),
        403,
        "POST /mcp with disallowed Host must return 403"
    );

    server_handle.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_resources_list_and_read() {
    use std::collections::HashSet;

    const KBS: &[&str] = &["alpha", "beta", "gamma"];
    const MAX_PAGES: u32 = 30;
    const OBJECTS_PER_KB: usize = 150;
    const BINARY_KB: &str = "alpha";
    const BINARY_KEY: &str = "binary-data.bin";

    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_kbs_and_mcp_http(KBS, http_addr);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();

    let mut seeded_uris: HashSet<String> = HashSet::new();
    let mut req_id: u64 = 0;
    for kb in KBS {
        for i in 0..OBJECTS_PER_KB {
            req_id += 1;
            let resp = mcp_request(
                &client,
                &mcp_url,
                req_id,
                "tools/call",
                serde_json::json!({
                    "name": "write",
                    "arguments": {
                        "kb": kb,
                        "path": format!("obj-{i:04}.md"),
                        "content": format!("# Object {i}\n\nThis is object {i} in knowledge base {kb}."),
                        "mime_type": "text/markdown"
                    }
                }),
            )
            .await;
            assert!(
                resp["error"].is_null(),
                "MCP write returned JSON-RPC error for {kb}/obj-{i:04}.md: {resp}"
            );
            assert!(
                !resp["result"]["isError"].as_bool().unwrap_or(false),
                "MCP write returned a tool-level error for {kb}/obj-{i:04}.md: {resp}"
            );
            seeded_uris.insert(format!("notedthat://{kb}/obj-{i:04}.md"));
        }
    }

    // The MCP write tool accepts only UTF-8 strings, so binary content is seeded
    // directly through the HTTP API.
    let binary_bytes: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0xAB, 0xCD, 0xEF, 0x01, 0x80];
    let binary_put = client
        .put(format!(
            "{http_url}/api/v1/knowledgebases/{BINARY_KB}/{BINARY_KEY}"
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "application/octet-stream")
        .body(binary_bytes)
        .send()
        .await
        .expect("PUT binary blob to HTTP API failed");
    assert!(
        binary_put.status().is_success(),
        "binary blob PUT must succeed, got {}",
        binary_put.status()
    );
    seeded_uris.insert(format!("notedthat://{BINARY_KB}/{BINARY_KEY}"));

    let mut all_uris: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page_count: u32 = 0;

    loop {
        page_count += 1;
        assert!(
            page_count <= MAX_PAGES,
            "resources/list cursor loop exceeded {MAX_PAGES} pages — probable infinite-loop bug"
        );

        req_id += 1;
        let params = match &cursor {
            Some(c) => serde_json::json!({ "cursor": c }),
            None => serde_json::json!({}),
        };
        let resp = mcp_request(&client, &mcp_url, req_id, "resources/list", params).await;
        assert!(
            resp["error"].is_null(),
            "resources/list returned JSON-RPC error on page {page_count}: {resp}"
        );

        let resources = resp["result"]["resources"]
            .as_array()
            .expect("resources/list result.resources must be a JSON array");

        for resource in resources {
            let uri = resource["uri"]
                .as_str()
                .expect("each resource must have a string 'uri' field")
                .to_string();
            all_uris.push(uri);
        }

        cursor = resp["result"]["nextCursor"].as_str().map(String::from);
        if cursor.is_none() {
            break;
        }
    }

    let uri_set: HashSet<&str> = all_uris.iter().map(String::as_str).collect();

    assert_eq!(
        uri_set.len(),
        all_uris.len(),
        "resources/list returned {} duplicate URI(s) ({} unique out of {} total across {} pages)",
        all_uris.len() - uri_set.len(),
        uri_set.len(),
        all_uris.len(),
        page_count
    );

    assert!(
        all_uris.len() >= seeded_uris.len(),
        "resources/list returned {} URIs but {} were seeded — drop detected",
        all_uris.len(),
        seeded_uris.len()
    );

    for seeded_uri in &seeded_uris {
        assert!(
            uri_set.contains(seeded_uri.as_str()),
            "seeded URI missing from resources/list result: {seeded_uri}"
        );
    }

    for uri in &all_uris {
        assert!(
            uri.starts_with("notedthat://"),
            "every resource URI must start with notedthat://: {uri}"
        );
        let (kb_part, obj_part) = uri
            .strip_prefix("notedthat://")
            .and_then(|s| s.split_once('/'))
            .unwrap_or_else(|| panic!("resource URI must have <kb>/<key> after scheme: {uri}"));
        assert!(
            KBS.contains(&kb_part),
            "URI KB slug {kb_part:?} must be one of {KBS:?}: {uri}"
        );
        assert!(
            !obj_part.is_empty(),
            "URI object key must not be empty: {uri}"
        );
    }

    let md_uri = "notedthat://alpha/obj-0000.md";
    req_id += 1;
    let read_md = mcp_request(
        &client,
        &mcp_url,
        req_id,
        "resources/read",
        serde_json::json!({ "uri": md_uri }),
    )
    .await;
    assert!(
        read_md["error"].is_null(),
        "resources/read for markdown URI must not return a JSON-RPC error: {read_md}"
    );
    let md_contents = read_md["result"]["contents"]
        .as_array()
        .expect("resources/read result.contents must be a JSON array");
    assert_eq!(
        md_contents.len(),
        1,
        "resources/read for a markdown object must return exactly 1 content item"
    );
    let md_item = &md_contents[0];
    assert_eq!(
        md_item["mimeType"].as_str(),
        Some("text/markdown"),
        "markdown resource must carry text/markdown MIME type: {md_item}"
    );
    assert!(
        md_item["text"].as_str().is_some(),
        "markdown resource must have a 'text' field (TextResourceContents): {md_item}"
    );
    assert!(
        md_item["blob"].is_null(),
        "markdown resource must not have a 'blob' field: {md_item}"
    );

    let bin_uri = format!("notedthat://{BINARY_KB}/{BINARY_KEY}");
    req_id += 1;
    let read_bin = mcp_request(
        &client,
        &mcp_url,
        req_id,
        "resources/read",
        serde_json::json!({ "uri": bin_uri }),
    )
    .await;
    assert!(
        read_bin["error"].is_null(),
        "resources/read for binary URI must not return a JSON-RPC error: {read_bin}"
    );
    let bin_contents = read_bin["result"]["contents"]
        .as_array()
        .expect("resources/read result.contents must be a JSON array");
    assert_eq!(
        bin_contents.len(),
        1,
        "resources/read for a binary object must return exactly 1 content item"
    );
    let bin_item = &bin_contents[0];
    assert_eq!(
        bin_item["mimeType"].as_str(),
        Some("application/octet-stream"),
        "binary resource must carry application/octet-stream MIME type: {bin_item}"
    );
    assert!(
        bin_item["blob"].as_str().is_some(),
        "binary resource must have a 'blob' (base64) field (BlobResourceContents): {bin_item}"
    );
    assert!(
        bin_item["text"].is_null(),
        "binary resource must not have a 'text' field: {bin_item}"
    );

    server_handle.abort();
}

#[tokio::test]
async fn mcp_replace_after_http_write_updates_content_and_advances_etag() {
    // Given: MCP HTTP and the HTTP API are running over the in-process backends.
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_mcp_http(http_addr);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();

    // When: HTTP PUT writes initial content and captures ETag.
    let put_response = client
        .put(format!(
            "{http_url}/api/v1/knowledgebases/notes/mcp-replace.md"
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "text/markdown")
        .body("hello world")
        .send()
        .await
        .expect("HTTP PUT failed");
    assert_eq!(put_response.status(), reqwest::StatusCode::CREATED);
    let first_etag = put_response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("PUT response must include ETag")
        .to_owned();

    // Initialize MCP.
    let initialize = mcp_request(
        &client,
        &mcp_url,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-e2e", "version": "0" }
        }),
    )
    .await;
    assert!(
        initialize.get("result").is_some(),
        "initialize should succeed: {initialize}"
    );

    // When: MCP replace tool is called with the first ETag.
    let replace_response = mcp_call_tool(
        &client,
        &mcp_url,
        1,
        "replace",
        serde_json::json!({
            "kb": "notes",
            "path": "mcp-replace.md",
            "old_string": "world",
            "new_string": "planet",
            "if_match": first_etag,
        }),
    )
    .await;
    assert!(
        replace_response.get("result").is_some(),
        "MCP replace should succeed: {replace_response}"
    );

    // Then: HTTP GET verifies the content was updated.
    let get_response = client
        .get(format!(
            "{http_url}/api/v1/knowledgebases/notes/mcp-replace.md"
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("HTTP GET failed");
    assert_eq!(get_response.status(), reqwest::StatusCode::OK);
    let updated_content = get_response
        .text()
        .await
        .expect("GET response body should read");
    assert_eq!(
        updated_content, "hello planet",
        "content should be updated after MCP replace"
    );

    server_handle.abort();
}
