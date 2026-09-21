//! Writes made over MCP reach search, and deletes leave it: the tool surface
//! and the indexer agree, driven at `POST /mcp` on a real server.

#![allow(dead_code, missing_docs)]
// allow: SIZE_OK — task requires duplicating the container-backed MCP E2E fixture here.

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
fn unique_phrase(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    format!("{prefix}_{nanos}")
}

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
    kbs.insert(
        "notes".to_string(),
        KbSlug::try_new("notes").expect("valid KB slug"),
    );

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
    let _guard = test_mutex().lock().await;
    let http_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config(http_addr);

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

/// Poll MCP search until a hit for `phrase` appears in KB `kb`, or timeout.
async fn poll_mcp_search(mcp: &McpSession, kb: &str, phrase: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let mut id = 100_u64;
    while start.elapsed() < timeout {
        let resp = mcp
            .call_tool(
                id,
                "search",
                &serde_json::json!({
                    "kb": [kb],
                    "query": phrase,
                    "limit": 5,
                }),
            )
            .await;
        id += 1;

        if search_response_contains_phrase(&resp, phrase) {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn poll_mcp_search_gone(mcp: &McpSession, kb: &str, phrase: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let mut id = 200_u64;
    while start.elapsed() < timeout {
        let resp = mcp
            .call_tool(
                id,
                "search",
                &serde_json::json!({
                    "kb": [kb],
                    "query": phrase,
                    "limit": 5,
                }),
            )
            .await;
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
        v["results"][0]["hits"].as_array().map(Vec::len)
    })
}

fn text_contains_hits(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v["results"][0]["hits"]
                .as_array()
                .map(|hits| !hits.is_empty())
        })
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_write_becomes_searchable_via_mcp_search() {
    // Given: a fresh NotedThat server and MCP client with a unique markdown document.
    let fixture = start_notedthat_server_fixture().await;
    let unique_phrase = unique_phrase("UNIQUE_MCP_CROSS_SURFACE");
    let path = format!("cross-surface-{unique_phrase}.md");
    let content = format!("# Cross-surface test\n{unique_phrase}\nThis document is indexed.");
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    // When: MCP writes the document.
    let write_resp = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": path,
                "content": content,
                "mime_type": "text/markdown",
            }),
        )
        .await;
    assert!(
        write_resp.get("error").is_none(),
        "write failed: {write_resp}"
    );

    // Then: MCP search returns that document after the indexer observes the write.
    let found = poll_mcp_search(&mcp, "notes", &unique_phrase, Duration::from_secs(10)).await;
    assert!(
        found,
        "search did not return the written document within 10s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_delete_removes_from_search() {
    // Given: a fresh NotedThat server and a unique markdown document written via MCP.
    let fixture = start_notedthat_server_fixture().await;
    let unique_phrase = unique_phrase("UNIQUE_DELETE_TEST");
    let path = format!("delete-{unique_phrase}.md");
    let content = format!("# Delete test\n{unique_phrase}");
    let mcp = McpSession::connect(&fixture.http_url, fixture.token);
    mcp.session_init().await;

    let write_resp = mcp
        .call_tool(
            1,
            "write",
            &serde_json::json!({
                "kb": "notes",
                "path": path,
                "content": content,
                "mime_type": "text/markdown",
            }),
        )
        .await;
    assert!(
        write_resp.get("error").is_none(),
        "write failed: {write_resp}"
    );

    let found = poll_mcp_search(&mcp, "notes", &unique_phrase, Duration::from_secs(10)).await;
    assert!(
        found,
        "search did not return the written document within 10s"
    );

    // When: MCP deletes the document.
    let delete_resp = mcp
        .call_tool(
            2,
            "delete",
            &serde_json::json!({
                "kb": "notes",
                "path": path,
            }),
        )
        .await;
    assert!(
        delete_resp.get("error").is_none(),
        "delete failed: {delete_resp}"
    );

    // Then: MCP search stops returning hits for the unique phrase.
    let gone = poll_mcp_search_gone(&mcp, "notes", &unique_phrase, Duration::from_secs(10)).await;
    assert!(gone, "search still returned the deleted document after 10s");
}
