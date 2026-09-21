//! The MCP tool surface, end to end over the streamable HTTP transport: a
//! real `notedthat-server` on in-process backends, driven at `POST /mcp`.

#![allow(dead_code, missing_docs)]
// allow: SIZE_OK — task requires duplicating the container-backed server fixture here.

use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::Mutex;

#[path = "support/mcp_http.rs"]
mod mcp_http;
use mcp_http::McpSession;

/// How long to wait for the server to bind after startup.
///
/// Generous on purpose. Startup still does real provisioning work — buckets,
/// manifests and a vector-store collection with its payload indexes — only now
/// against in-process backends rather than containers. A loaded CI runner can
/// still be slow enough that a tight budget produces an intermittent failure
/// instead of a real signal, which is what the old 10s budget did.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(60);

const API_TOKEN: &str = "e2e-test-token";

// ─── Server Fixture (mirrors webdav_cross_surface_e2e.rs) ───────────────────

/// Vector width the stub embedder and the provisioned collection agree on.
const EMBEDDING_DIM: u32 = 4;

/// Storage, vector store and embedder, all in-process.
///
/// These tests drive `POST /mcp` on a real server; only the three external
/// services behind that server are substituted.
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

fn test_config(http_addr: std::net::SocketAddr) -> notedthat_server::config::Config {
    use notedthat_core::{KbSlug, StagingConfig, TenantSlug};
    use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
    use std::collections::BTreeMap;

    let mut kbs = BTreeMap::new();
    kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: http_addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        events: notedthat_server::config::EventsConfig::None,
        staging: StagingConfig::default(),
        oidc: None,
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
        max_patchable_size: 100 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
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

struct NotedThatServerFixture {
    http_url: String,
    token: &'static str,
    server_handle: tokio::task::JoinHandle<()>,
}

impl Drop for NotedThatServerFixture {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

async fn start_notedthat_server_fixture() -> NotedThatServerFixture {
    start_notedthat_server_fixture_with(|_| {}).await
}

/// A server whose `Config` is adjusted before it starts — a read budget, say.
async fn start_notedthat_server_fixture_with(
    adjust: impl FnOnce(&mut notedthat_server::config::Config),
) -> NotedThatServerFixture {
    let _guard = test_mutex().lock().await;
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let mut config = test_config(http_addr);
    adjust(&mut config);

    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    wait_for_http(&format!("{http_url}/healthz"), SERVER_READY_TIMEOUT).await;

    NotedThatServerFixture {
        http_url,
        token: API_TOKEN,
        server_handle,
    }
}

static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn test_mutex() -> &'static Mutex<()> {
    TEST_MUTEX.get_or_init(|| Mutex::new(()))
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_infra_smoke() {
    // Given: a real server with `/mcp` mounted, and a client with its credential.
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);

    // When: the client sends the MCP initialize request.
    let resp = mcp.initialize().await;

    // Then: the server responds with a JSON-RPC result.
    assert!(resp.get("result").is_some(), "expected result: {resp}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_initialize_returns_valid_response() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);

    let resp = mcp.initialize().await;
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_tools_list_returns_all_ten() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    // Request tools/list
    let resp = mcp.request(1, "tools/list", &serde_json::json!({})).await;

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
        "replace",
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
        10,
        "expected exactly 10 tools, got {}: {actual_tools:?}",
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
}

async fn authenticated_object_state(
    fixture: &NotedThatServerFixture,
    path: &str,
) -> (Vec<u8>, String, String) {
    let response = reqwest::Client::new()
        .get(format!(
            "{}/api/v1/knowledgebases/notes/{path}",
            fixture.http_url
        ))
        .bearer_auth(fixture.token)
        .send()
        .await
        .expect("authenticated object read")
        .error_for_status()
        .expect("object read succeeds");
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .expect("content type header")
        .to_str()
        .expect("content type is valid")
        .to_string();
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .expect("etag header")
        .to_str()
        .expect("etag is valid")
        .to_string();
    let body = response.bytes().await.expect("object body").to_vec();
    (body, content_type, etag)
}

// ─── W4.3: Happy-path chain ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn mcp_write_list_read_delete() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    // 1. Write a note.
    let write_resp = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": "test-w4.md",
                "content": "# Test\nHello from W4.3",
            }),
        )
        .await;
    assert!(
        write_resp.get("result").is_some(),
        "write should succeed: {write_resp}"
    );

    // 2. List with prefix — test-w4.md must appear.
    let list_resp = mcp
        .call_tool(
            2,
            "list",
            &serde_json::json!({ "kb": "notes", "prefix": "test-" }),
        )
        .await;
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
    let read_resp = mcp
        .call_tool(
            3,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "test-w4.md" }),
        )
        .await;
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
    let range_resp = mcp
        .call_tool(
            4,
            "read",
            &serde_json::json!({
                "kb": "notes",
                "path": "test-w4.md",
                "byte_start": 0,
                "byte_end": 6,
            }),
        )
        .await;
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
    let del_resp = mcp
        .call_tool(
            5,
            "delete",
            &serde_json::json!({ "kb": "notes", "path": "test-w4.md" }),
        )
        .await;
    assert!(
        del_resp.get("result").is_some(),
        "delete should succeed: {del_resp}"
    );

    // 6. Read after delete → not_found error.
    let gone_resp = mcp
        .call_tool(
            6,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "test-w4.md" }),
        )
        .await;
    assert!(
        gone_resp.get("error").is_some(),
        "read of deleted note should return error: {gone_resp}"
    );
    let msg = gone_resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not_found"),
        "expected not_found in error message, got: {msg:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_move_happy() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    // 1. Write source note.
    let write_resp = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": "src-move.md",
                "content": "# Move source",
            }),
        )
        .await;
    assert!(
        write_resp.get("result").is_some(),
        "write src should succeed: {write_resp}"
    );

    // 2. Move src-move.md → dst-move.md.
    let move_resp = mcp
        .call_tool(
            2,
            "move",
            &serde_json::json!({
                "kb": "notes",
                "from": "src-move.md",
                "to": "dst-move.md",
            }),
        )
        .await;
    assert!(
        move_resp.get("result").is_some(),
        "move should succeed: {move_resp}"
    );

    // 3. Read destination — must contain original content.
    let read_dst = mcp
        .call_tool(
            3,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "dst-move.md" }),
        )
        .await;
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
    let read_src = mcp
        .call_tool(
            4,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "src-move.md" }),
        )
        .await;
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
    let del_resp = mcp
        .call_tool(
            5,
            "delete",
            &serde_json::json!({ "kb": "notes", "path": "dst-move.md" }),
        )
        .await;
    assert!(
        del_resp.get("result").is_some(),
        "delete dst should succeed: {del_resp}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_move_self_rejected_without_mutating_source() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    let source_path = "self-move.md";
    let source_content = "self-move source content";
    let write_resp = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": source_path,
                "content": source_content,
                "mime_type": "text/plain",
            }),
        )
        .await;
    assert!(
        write_resp.get("result").is_some(),
        "source write should succeed: {write_resp}"
    );
    let before = authenticated_object_state(&fixture, source_path).await;

    for (id, to) in [(2, source_path), (3, "/self-move.md")] {
        let response = mcp
            .call_tool(
                id,
                "move",
                &serde_json::json!({
                    "kb": "notes",
                    "from": source_path,
                    "to": to,
                }),
            )
            .await;
        assert_eq!(
            response["error"]["code"].as_i64(),
            Some(-32602),
            "self-move should be invalid params: {response}"
        );
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("paths resolve to the same object")),
            "self-move should name the normalized-path conflict: {response}"
        );
    }

    let read_resp = mcp
        .call_tool(
            4,
            "read",
            &serde_json::json!({ "kb": "notes", "path": source_path }),
        )
        .await;
    assert_eq!(
        read_resp["result"]["content"][0]["text"].as_str(),
        Some(source_content),
        "MCP read must retain the source bytes: {read_resp}"
    );
    assert_eq!(
        authenticated_object_state(&fixture, source_path).await,
        before,
        "authenticated HTTP read must retain bytes, content type, and content-derived ETag"
    );

    let del_resp = mcp
        .call_tool(
            5,
            "delete",
            &serde_json::json!({ "kb": "notes", "path": source_path }),
        )
        .await;
    assert!(
        del_resp.get("result").is_some(),
        "cleanup should succeed: {del_resp}"
    );
}

// ─── W4.4: Error paths ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_read_missing() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    let resp = mcp
        .call_tool(
            1,
            "read",
            &serde_json::json!({
                "kb": "notes",
                "path": "does-not-exist-w4.md",
            }),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "read of missing object should error: {resp}"
    );
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not_found"),
        "expected not_found in message, got: {msg:?}"
    );
}

/// The read→edit workflow with no second round trip: the `ETag` in the read's
/// `structuredContent` is the version the text came from, `edit` accepts it,
/// and after someone else writes, the same token is refused rather than
/// applied to a version the text never described (#86).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn mcp_read_carries_the_etag_its_text_belongs_to() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    let written = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": "read-meta.md",
                "content": "one\ntwo\nthree\n",
            }),
        )
        .await;
    assert!(written.get("result").is_some(), "write: {written}");

    // A full read: text in content, the version and totals beside it.
    let read = mcp
        .call_tool(
            2,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "read-meta.md" }),
        )
        .await;
    assert_eq!(read["result"]["content"][0]["text"], "one\ntwo\nthree\n");
    let meta = &read["result"]["structuredContent"];
    let (_, _, http_etag) = authenticated_object_state(&fixture, "read-meta.md").await;
    assert_eq!(
        meta["etag"], http_etag,
        "the etag the HTTP API reports: {meta}"
    );
    assert_eq!(meta["bytes_returned"], 14);
    assert_eq!(meta["total_bytes"], 14);
    assert_eq!(meta["byte_start"], 0);
    assert_eq!(meta["byte_end"], 14);
    assert!(
        meta["total_lines"].is_null(),
        "no line total on a full read: {meta}"
    );

    // A line read reports the slice's lines and the object's totals.
    let lines = mcp
        .call_tool(
            3,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "read-meta.md", "line_start": 2, "line_end": 2 }),
        )
        .await;
    assert_eq!(lines["result"]["content"][0]["text"], "two\n");
    let line_meta = &lines["result"]["structuredContent"];
    assert_eq!(line_meta["etag"], http_etag);
    assert_eq!(line_meta["total_lines"], 3, "{line_meta}");
    assert_eq!(line_meta["line_start"], 2);
    assert_eq!(line_meta["line_end"], 2);
    assert_eq!(line_meta["total_bytes"], 14);
    assert_eq!(line_meta["byte_start"], 4);
    assert_eq!(line_meta["byte_end"], 8);
    assert_eq!(line_meta["bytes_returned"], 4);

    // That etag is exactly what edit wants.
    let etag = meta["etag"].as_str().expect("etag string").to_string();
    let edited = mcp
        .call_tool(
            4,
            "edit",
            &serde_json::json!({
                "kb": "notes",
                "path": "read-meta.md",
                "line_start": 2,
                "line_end": 2,
                "content": "TWO\n",
                "if_match": etag,
            }),
        )
        .await;
    assert!(
        edited.get("result").is_some(),
        "edit with the read's etag: {edited}"
    );

    // Someone else writes; the token from the earlier read now names a stale version.
    reqwest::Client::new()
        .put(format!(
            "{}/api/v1/knowledgebases/notes/read-meta.md",
            fixture.http_url
        ))
        .bearer_auth(fixture.token)
        .header("content-type", "text/markdown")
        .body("someone else's version\n")
        .send()
        .await
        .expect("intervening write")
        .error_for_status()
        .expect("intervening write succeeds");
    let stale = mcp
        .call_tool(
            5,
            "edit",
            &serde_json::json!({
                "kb": "notes",
                "path": "read-meta.md",
                "line_start": 1,
                "line_end": 1,
                "content": "clobber\n",
                "if_match": etag,
            }),
        )
        .await;
    let msg = stale["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("precondition_failed"),
        "a stale etag must be refused, got: {stale}"
    );
}

/// An object over the server's read budget is refused with the way through,
/// and a slice inside the budget is served (#88). The budget is the server's
/// `NOTEDTHAT_MCP_MAX_READ_BYTES`, set on its `Config` here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_read_over_the_budget_is_redirected_to_slices() {
    let fixture = start_notedthat_server_fixture_with(|config| config.mcp_max_read_bytes = 64).await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    let body = "x".repeat(200);
    let written = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({ "kb": "notes", "path": "big.md", "content": body }),
        )
        .await;
    assert!(written.get("result").is_some(), "write: {written}");

    let whole = mcp
        .call_tool(
            2,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "big.md" }),
        )
        .await;
    let msg = whole["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.starts_with("response_too_large: the object is 200 bytes"),
        "{whole}"
    );
    assert!(msg.contains("64 bytes"), "{msg}");
    assert!(msg.contains("byte_start/byte_end"), "{msg}");

    let slice = mcp
        .call_tool(
            3,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "big.md", "byte_start": 0, "byte_end": 64 }),
        )
        .await;
    assert_eq!(
        slice["result"]["content"][0]["text"],
        "x".repeat(64),
        "{slice}"
    );
    assert_eq!(slice["result"]["structuredContent"]["total_bytes"], 200);
    assert_eq!(slice["result"]["structuredContent"]["byte_end"], 64);

    let resource = mcp
        .request(
            4,
            "resources/read",
            &serde_json::json!({ "uri": "notedthat://notes/big.md" }),
        )
        .await;
    let msg = resource["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("response_too_large"), "{resource}");
    assert!(msg.contains("read tool"), "{msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_write_precondition() {
    let fixture = start_notedthat_server_fixture().await;
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    // 1. Initial write — capture the returned etag.
    let first = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": "test-precond.md",
                "content": "v1",
            }),
        )
        .await;
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
    let second = mcp
        .call_tool(
            2,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": "test-precond.md",
                "content": "v2",
                "if_match": wrong_etag,
            }),
        )
        .await;
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
    let _ = mcp
        .call_tool(
            3,
            "delete",
            &serde_json::json!({ "kb": "notes", "path": "test-precond.md" }),
        )
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_bad_token() {
    let fixture = start_notedthat_server_fixture().await;
    // Intentionally pass a wrong token — the transport refuses every request
    // before it reaches a tool, `initialize` included.
    let mcp = McpSession::connect(&fixture.http_url, "wrong-token");

    let response = mcp
        .send(
            1,
            "tools/call",
            &serde_json::json!({ "name": "list_knowledgebases", "arguments": {} }),
        )
        .await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a bad bearer is refused at the transport"
    );
    let body: serde_json::Value = response.json().await.expect("401 body is JSON");
    assert_eq!(body["error"], "unauthorized", "{body}");
}
