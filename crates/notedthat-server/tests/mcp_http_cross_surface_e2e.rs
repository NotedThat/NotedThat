#![allow(missing_docs)]

use std::time::Duration;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const API_TOKEN: &str = "e2e-test-token";
const EXPECTED_M7_TOOLS: &str = "list_knowledgebases,search,read,write,list,delete,move";

// SeaweedFS 4.18 requires an IAM config file to accept signed S3 requests without this the
// S3 gateway rejects all signed requests. target path is the FIRST arg in with_copy_to.
const SEAWEEDFS_S3_CONFIG: &[u8] = br#"{"identities":[{"name":"test","credentials":[{"accessKey":"any","secretKey":"any"}],"actions":["Admin","Read","Write","List","Tagging"]}]}"#;

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

fn test_config_with_mcp_http(
    http_addr: std::net::SocketAddr,
    dav_addr: std::net::SocketAddr,
    mcp_addr: std::net::SocketAddr,
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
        mcp_http_bind: mcp_addr,
        mcp_http_enabled: true,
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
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
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("MCP tool result must contain JSON text content");
    serde_json::from_str(text).expect("MCP JSON content must parse")
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
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn mcp_http_initialize_tools() {
    // Given: all three server listeners are configured on random loopback ports.
    let (_seaweed, s3_url) = start_seaweedfs().await;
    let (_qdrant, qdrant_url) = start_qdrant().await;
    let mock_embedder = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_embedder)
        .await;

    let http_addr = free_addr().await;
    let dav_addr = free_addr().await;
    let mcp_addr = free_addr().await;
    let config = test_config_with_mcp_http(
        http_addr,
        dav_addr,
        mcp_addr,
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
    let mcp_url = format!("http://{mcp_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), Duration::from_secs(10)).await;

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
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn mcp_http_write_search_identity() {
    // Given: MCP HTTP, HTTP API, SeaweedFS, Qdrant, and the mock embedder are running.
    let (_seaweed, s3_url) = start_seaweedfs().await;
    let (_qdrant, qdrant_url) = start_qdrant().await;
    let mock_embedder = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_embedder)
        .await;

    let http_addr = free_addr().await;
    let dav_addr = free_addr().await;
    let mcp_addr = free_addr().await;
    let config = test_config_with_mcp_http(
        http_addr,
        dav_addr,
        mcp_addr,
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
    let mcp_url = format!("http://{mcp_addr}/mcp");
    wait_for_http(&format!("{http_url}/healthz"), Duration::from_secs(10)).await;

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
    let embedder_requests = mock_embedder.received_requests().await.unwrap_or_default();
    eprintln!(
        "embedder wiremock hits before MCP search assertion: {}",
        embedder_requests.len()
    );
    for (index, request) in embedder_requests.iter().enumerate() {
        eprintln!(
            "embedder wiremock hit {index}: method={} path={} body={}",
            request.method,
            request.url.path(),
            String::from_utf8_lossy(&request.body),
        );
    }

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
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn mcp_http_auth_and_sse_refusal() {
    // Given: all three server listeners configured on random loopback ports.
    let (_seaweed, s3_url) = start_seaweedfs().await;
    let (_qdrant, qdrant_url) = start_qdrant().await;
    let mock_embedder = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_embedder)
        .await;

    let http_addr = free_addr().await;
    let dav_addr = free_addr().await;
    let mcp_addr = free_addr().await;
    let config = test_config_with_mcp_http(
        http_addr,
        dav_addr,
        mcp_addr,
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
    let mcp_url = format!("http://{mcp_addr}/mcp");
    let sse_url = format!("http://{mcp_addr}/sse");
    wait_for_http(&format!("{http_url}/healthz"), Duration::from_secs(10)).await;

    // Raw reqwest client — no MCP library, no redirect following.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client");

    const EXACT_SSE_REFUSAL_BODY: &str = r#"{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}"#;

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

    //    SSE refusal fires before auth, so no Authorization header needed.
    let resp = client
        .post(&sse_url)
        .header("Content-Type", "application/json")
        .body(r#"{}"#)
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

    let resp = client
        .get(&mcp_url)
        .send()
        .await
        .expect("GET /mcp failed");
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
