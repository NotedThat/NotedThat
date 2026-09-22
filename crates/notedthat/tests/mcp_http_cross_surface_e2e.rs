#![allow(missing_docs)]

use std::time::Duration;

use notedthat_mcp::testing::McpSession;

/// How long to wait for the server to bind after startup begins.
///
/// Provisioning is in-process now, so this is generous by a wide margin; it
/// exists to fail with a clear message rather than hang if startup breaks.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);

const API_TOKEN: &str = "e2e-test-token";
const EXPECTED_M7_TOOLS: &str =
    "list_knowledgebases,search,read,write,list,delete,move,append,edit,replace,index_status";

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
        events: None,
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
        events: notedthat_server::config::EventsConfig::None,
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
        mcp_anonymous: notedthat_server::config::McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        ready_probe_interval_ms: 5_000,
        staging: notedthat_core::StagingConfig::default(),
        oidc: None,
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
        events: notedthat_server::config::EventsConfig::None,
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
        mcp_anonymous: notedthat_server::config::McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        ready_probe_interval_ms: 5_000,
        staging: notedthat_core::StagingConfig::default(),
        oidc: None,
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
    mcp: &McpSession,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    mcp.request(id, method, &params).await
}

async fn mcp_call_tool(
    mcp: &McpSession,
    id: u64,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    mcp.call_tool(id, tool_name, &arguments).await
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

async fn poll_mcp_search_hit(
    mcp: &McpSession,
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
            mcp,
            id,
            "search",
            serde_json::json!({
                "kb": ["notes"],
                "query": phrase,
                "limit": 5,
            }),
        )
        .await;
        id += 1;

        if response.get("error").is_none() {
            let search_result = mcp_json_content(&response);
            if let Some(hit) = search_result["results"][0]["hits"]
                .as_array()
                .and_then(|hits| {
                    hits.iter()
                        .find(|hit| hit["object_key"].as_str() == Some("e2e.md"))
                })
            {
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
    let mcp = McpSession::connect(&format!("http://{http_addr}"), API_TOKEN);
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    // When: an authenticated MCP Streamable HTTP client initializes and lists tools.
    let initialize = mcp_request(
        &mcp,
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

    let tools_list = mcp_request(&mcp, 1, "tools/list", serde_json::json!({})).await;
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
    let mcp = McpSession::connect(&format!("http://{http_addr}"), API_TOKEN);
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    let phrase = format!("notedthat_m8_http_write_search_unique_{nonce}");
    let content = format!("# MCP HTTP identity\n\n{phrase}\n");

    let initialize = mcp_request(
        &mcp,
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
        &mcp,
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

    let found_hit = poll_mcp_search_hit(&mcp, &phrase, Duration::from_secs(40)).await;
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

/// Poll MCP `search` with `arguments` until `ready` accepts the answer, or
/// give up at `timeout`. A JSON-RPC error is never accepted.
async fn poll_mcp_search_until(
    mcp: &McpSession,
    arguments: serde_json::Value,
    timeout: Duration,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut id = 500_u64;
    loop {
        if tokio::time::Instant::now() > deadline {
            return None;
        }
        let response = mcp_call_tool(mcp, id, "search", arguments.clone()).await;
        id += 1;
        if response.get("error").is_none() {
            let answer = mcp_json_content(&response);
            if ready(&answer) {
                return Some(answer);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn mcp_http_search_spans_knowledge_bases() {
    // Given: two knowledge bases, each holding one document, all in-process
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_kbs_and_mcp_http(&["alpha", "beta"], http_addr);
    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });
    let http_url = format!("http://{http_addr}");
    let mcp = McpSession::connect(&format!("http://{http_addr}"), API_TOKEN);
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();
    for kb in ["alpha", "beta"] {
        let put = client
            .put(format!("{http_url}/api/v1/knowledgebases/{kb}/{kb}.md"))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Content-Type", "text/markdown")
            .body(format!("# {kb}\n\nthe definition of a document in {kb}\n"))
            .send()
            .await
            .expect("PUT should send");
        assert!(put.status().is_success(), "PUT into {kb}: {}", put.status());
    }
    let initialize = mcp_request(
        &mcp,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-e2e", "version": "0" }
        }),
    )
    .await;
    assert!(initialize.get("result").is_some(), "{initialize}");

    let both_have_a_hit = |answer: &serde_json::Value| {
        answer["results"].as_array().is_some_and(|groups| {
            groups.len() == 2
                && groups
                    .iter()
                    .all(|g| !g["hits"].as_array().unwrap_or(&vec![]).is_empty())
        })
    };

    // When: one call names both knowledge bases, beta first
    let answer = poll_mcp_search_until(
        &mcp,
        serde_json::json!({"kb": ["beta", "alpha"], "query": "definition of a document", "limit": 5}),
        Duration::from_secs(40),
        both_have_a_hit,
    )
    .await
    .expect("both knowledge bases should answer within 40 s");

    // Then: one group per knowledge base in request order, every hit naming
    // its knowledge base, nothing skipped
    let groups = answer["results"].as_array().unwrap();
    assert_eq!(groups[0]["kb"], "beta", "{answer}");
    assert_eq!(groups[1]["kb"], "alpha", "{answer}");
    assert_eq!(groups[0]["hits"][0]["kb"], "beta", "{answer}");
    assert_eq!(groups[0]["hits"][0]["object_key"], "beta.md", "{answer}");
    assert_eq!(groups[1]["hits"][0]["kb"], "alpha", "{answer}");
    assert_eq!(groups[1]["hits"][0]["object_key"], "alpha.md", "{answer}");
    assert_eq!(answer["skipped"], serde_json::json!([]), "{answer}");

    // When: the list names a knowledge base the server has not declared
    let refused = mcp_call_tool(
        &mcp,
        1,
        "search",
        serde_json::json!({"kb": ["alpha", "nope"], "query": "definition of a document"}),
    )
    .await;

    // Then: the whole call fails, and the error names the slug
    let message = refused["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("an unknown slug should fail the call: {refused}"));
    assert!(message.starts_with("not_found"), "{message}");
    assert!(message.contains("\"nope\""), "{message}");

    // When: the call names no knowledge base at all
    let answer = poll_mcp_search_until(
        &mcp,
        serde_json::json!({"query": "definition of a document"}),
        Duration::from_secs(10),
        both_have_a_hit,
    )
    .await
    .expect("an omitted kb should search every knowledge base");

    // Then: every declared knowledge base is searched, in the order the API
    // lists them, and none is skipped for the service token
    let groups = answer["results"].as_array().unwrap();
    assert_eq!(groups[0]["kb"], "alpha", "{answer}");
    assert_eq!(groups[1]["kb"], "beta", "{answer}");
    assert_eq!(answer["skipped"], serde_json::json!([]), "{answer}");

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
    // Bearer is accepted everywhere: a WebDAV client that can set a header need
    // not speak Basic, and an identity-provider token only exists as a bearer.
    // The asymmetry is deliberate — Basic is a WebDAV-only convenience.
    assert_eq!(
        resp.status().as_u16(),
        204,
        "the API Bearer credential authorizes WebDAV requests too"
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

    //    GET and DELETE /mcp are the stateful transport's own (D66): the
    //    notification leg and the session end. Auth is outermost, so without a
    //    credential they are 401 like POST; with one, rmcp wants a session —
    //    400 without the header, 404 for an id it never issued.
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let resp = client
            .request(method.clone(), &mcp_url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .expect("unauthenticated /mcp");
        assert_eq!(
            resp.status().as_u16(),
            401,
            "{method} /mcp without a credential"
        );

        let resp = client
            .request(method.clone(), &mcp_url)
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Accept", "text/event-stream")
            .send()
            .await
            .expect("sessionless /mcp");
        assert_eq!(
            resp.status().as_u16(),
            400,
            "{method} /mcp without a session"
        );

        let resp = client
            .request(method.clone(), &mcp_url)
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Accept", "text/event-stream")
            .header("Mcp-Session-Id", "no-such-session")
            .send()
            .await
            .expect("bogus-session /mcp");
        let expected = if method == reqwest::Method::GET {
            404
        } else {
            202
        };
        assert_eq!(
            resp.status().as_u16(),
            expected,
            "{method} /mcp with an unknown session"
        );
    }

    //    A request on no session, other than initialize, is refused by the
    //    transport before any handler runs.
    let resp = client
        .post(&mcp_url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#)
        .send()
        .await
        .expect("sessionless tools/list");
    assert_eq!(
        resp.status().as_u16(),
        422,
        "a non-initialize POST without Mcp-Session-Id"
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
    let mcp = McpSession::connect(&format!("http://{http_addr}"), API_TOKEN);
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    let client = reqwest::Client::new();

    let mut seeded_uris: HashSet<String> = HashSet::new();
    let mut req_id: u64 = 0;
    for kb in KBS {
        for i in 0..OBJECTS_PER_KB {
            req_id += 1;
            let resp = mcp_request(
                &mcp,
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
        let resp = mcp_request(&mcp, req_id, "resources/list", params).await;
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
        &mcp,
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
        &mcp,
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
#[allow(clippy::too_many_lines)]
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
    let mcp = McpSession::connect(&format!("http://{http_addr}"), API_TOKEN);
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
        &mcp,
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
        &mcp,
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

    // And over the HTTP transport too, a read's structuredContent names the
    // version its text came from — the one the replace just produced.
    let read_response = mcp_call_tool(
        &mcp,
        2,
        "read",
        serde_json::json!({ "kb": "notes", "path": "mcp-replace.md" }),
    )
    .await;
    assert_eq!(
        read_response["result"]["content"][0]["text"], "hello planet",
        "{read_response}"
    );
    let meta = &read_response["result"]["structuredContent"];
    let head = client
        .head(format!(
            "{http_url}/api/v1/knowledgebases/notes/mcp-replace.md"
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("HTTP HEAD failed");
    let current_etag = head
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("HEAD carries an ETag")
        .to_owned();
    assert_eq!(meta["etag"], current_etag, "{meta}");
    assert_ne!(meta["etag"], first_etag, "the replace advanced the etag");
    assert_eq!(meta["total_bytes"], 12, "{meta}");

    server_handle.abort();
}

// ─── Anonymous MCP (D59) ─────────────────────────────────────────────────────

/// A `POST /mcp` with no `Authorization` header at all; the raw answer.
async fn anonymous_mcp(
    anon: &McpSession,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> reqwest::Response {
    anon.send(id, method, &params).await
}

async fn anonymous_tool_call(
    anon: &McpSession,
    id: u64,
    tool: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    anon.call_tool(id, tool, &arguments).await
}

/// Two knowledge bases whose manifests are written before the server starts,
/// so they are its startup snapshot: `public` grants `anyone` `list`, `read`
/// and `search`; `private` grants anonymous callers nothing.
async fn seed_public_and_private(backends: &notedthat_server::run::Backends) {
    use notedthat_core::{AccessRule, KbManifest, KbSlug, TenantSlug, Verb, Who};

    for (slug, anonymous) in [("public", true), ("private", false)] {
        let kb = KbSlug::try_new(slug).expect("valid slug");
        backends.storage.ensure_bucket(&kb).await.expect("bucket");
        let mut manifest = KbManifest::new_v1(&TenantSlug::default(), &kb, slug, 1_700_000_000);
        let mut rules = vec![AccessRule::new(Who::SignedIn, Verb::ALL)];
        if anonymous {
            rules.push(AccessRule::new(
                Who::Anyone,
                [Verb::List, Verb::Read, Verb::Search],
            ));
        }
        manifest.access = rules.into_iter().collect();
        backends
            .storage
            .write_manifest(&kb, &manifest)
            .await
            .expect("manifest");
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn anonymous_mcp_is_bound_by_the_anyone_rules() {
    // Given: a deployment with one public and one private knowledge base
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config_with_kbs_and_mcp_http(&["public", "private"], http_addr);
    let backends = in_memory_backends();
    seed_public_and_private(&backends).await;
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });
    let http_url = format!("http://{http_addr}");
    let mcp_url = format!("http://{http_addr}/mcp");
    let anon = McpSession::anonymous(&format!("http://{http_addr}"));
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;
    let client = reqwest::Client::new();
    // One note in each, written through the API so that it is indexed too.
    for (kb, body) in [
        ("public", "# Public\n\nreadable by anyone\n"),
        ("private", "# Private\n\nsigned-in only\n"),
    ] {
        let response = client
            .put(format!("{http_url}/api/v1/knowledgebases/{kb}/note.md"))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Content-Type", "text/markdown")
            .body(body)
            .send()
            .await
            .expect("seed PUT");
        assert_eq!(response.status(), reqwest::StatusCode::CREATED, "{kb}");
    }

    // When / Then: capability discovery needs no credential — an anonymous
    // client opens a session like any other
    let initialized = anon.initialize().await;
    assert!(
        initialized.get("result").is_some(),
        "initialize: {initialized}"
    );
    assert!(anon.session_id().is_some(), "a session was issued");
    let tools = anon.request(2, "tools/list", &serde_json::json!({})).await;
    assert_eq!(
        tools["result"]["tools"].as_array().map(Vec::len),
        Some(EXPECTED_M7_TOOLS.split(',').count()),
        "every tool is advertised; a mutating call fails instead: {tools}"
    );

    // Then: discovery names only the knowledge base that grants anyone something
    let listed = anonymous_tool_call(&anon, 3, "list_knowledgebases", serde_json::json!({})).await;
    // Slugs only: an entry may carry more fields (#155 adds `display_name`
    // and `description`); what matters here is which knowledge bases appear.
    let entries = mcp_json_content(&listed);
    let slugs: Vec<&str> = entries
        .as_array()
        .expect("an array of entries")
        .iter()
        .map(|entry| entry["kb_slug"].as_str().expect("kb_slug"))
        .collect();
    assert_eq!(slugs, ["public"], "{listed}");

    // Then: read and list succeed where `anyone` holds the verb …
    let read = anonymous_tool_call(
        &anon,
        4,
        "read",
        serde_json::json!({ "kb": "public", "path": "note.md" }),
    )
    .await;
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("readable by anyone")),
        "{read}"
    );
    let list = anonymous_tool_call(&anon, 5, "list", serde_json::json!({ "kb": "public" })).await;
    assert_eq!(
        mcp_json_content(&list)["objects"][0]["key"],
        serde_json::json!("note.md"),
        "{list}"
    );

    // … and are concealed as not_found where it does not — the private
    // knowledge base is indistinguishable from one that was never declared
    for (id, tool, arguments) in [
        (
            6,
            "read",
            serde_json::json!({ "kb": "private", "path": "note.md" }),
        ),
        (7, "list", serde_json::json!({ "kb": "private" })),
        (
            8,
            "read",
            serde_json::json!({ "kb": "undeclared", "path": "note.md" }),
        ),
    ] {
        let denied = anonymous_tool_call(&anon, id, tool, arguments).await;
        assert_eq!(
            denied["error"]["code"],
            serde_json::json!(-32002),
            "{tool} on a knowledge base anyone may not touch is not_found: {denied}"
        );
        assert!(
            !denied.to_string().contains("forbidden"),
            "an anonymous denial never says forbidden: {denied}"
        );
    }

    // Then: a search with `kb` omitted covers what anyone may search and
    // never names the private knowledge base. Indexing is asynchronous, so
    // poll until the public note is a hit.
    let deadline = tokio::time::Instant::now() + SERVER_READY_TIMEOUT;
    let searched = loop {
        let answer = anonymous_tool_call(
            &anon,
            9,
            "search",
            serde_json::json!({ "query": "readable by anyone", "limit": 5 }),
        )
        .await;
        if answer.get("error").is_none() {
            let content = mcp_json_content(&answer);
            if content["results"][0]["hits"]
                .as_array()
                .is_some_and(|hits| !hits.is_empty())
            {
                break content;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "public note never became searchable anonymously: {answer}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(
        searched["results"].as_array().map(Vec::len),
        Some(1),
        "{searched}"
    );
    assert_eq!(searched["results"][0]["kb"], "public", "{searched}");
    assert!(
        !searched.to_string().contains("private"),
        "a knowledge base anyone may not see is never named, not even as skipped: {searched}"
    );

    // Then: a mutating tool is refused — its route admits no anonymous caller
    let write = anonymous_tool_call(
        &anon,
        10,
        "write",
        serde_json::json!({ "kb": "public", "path": "new.md", "content": "# no" }),
    )
    .await;
    assert_eq!(write["error"]["message"], "unauthorized", "{write}");
    let response = client
        .get(format!("{http_url}/api/v1/knowledgebases/public/new.md"))
        .send()
        .await
        .expect("GET");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "nothing was written"
    );

    // Then: a supplied credential that does not verify is refused outright,
    // never quietly downgraded to the anonymous caller
    let response = client
        .post(&mcp_url)
        .header("Authorization", "Bearer not-the-token")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":11,"method":"tools/list","params":{}}"#)
        .send()
        .await
        .expect("POST /mcp (wrong bearer)");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.expect("401 body");
    assert_eq!(body["error"], "unauthorized");

    server_handle.abort();
}

#[tokio::test]
async fn never_keeps_mcp_credentialed_on_a_public_deployment() {
    // Given: the same public knowledge base, and an operator who said never
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let mut config = test_config_with_kbs_and_mcp_http(&["public", "private"], http_addr);
    config.mcp_anonymous = notedthat_server::config::McpAnonymous::Never;
    let backends = in_memory_backends();
    seed_public_and_private(&backends).await;
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });
    let http_url = format!("http://{http_addr}");
    let anon = McpSession::anonymous(&format!("http://{http_addr}"));
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;
    let client = reqwest::Client::new();

    // When: an anonymous client tries to discover the tools
    let response = anonymous_mcp(&anon, 1, "tools/list", serde_json::json!({})).await;

    // Then: 401 — the challenge an OAuth-capable client needs — although the
    // same request on the HTTP API succeeds, because the rules did not change
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let response = client
        .get(format!("{http_url}/api/v1/knowledgebases"))
        .send()
        .await
        .expect("GET /api/v1/knowledgebases");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    server_handle.abort();
}
