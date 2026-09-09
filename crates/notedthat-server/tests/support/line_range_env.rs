use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{KbSlug, TenantSlug};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_server::run::Backends;
use reqwest::StatusCode;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long to wait for the server to bind after startup begins.
///
/// Provisioning is in-process now, so this is generous by a wide margin; it
/// exists to fail with a clear message rather than hang if startup breaks.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Vector width the stub embedder and the provisioned collection agree on.
const EMBEDDING_DIM: u32 = 4;

pub const API_TOKEN: &str = "e2e-test-token";
pub const TWENTY_LINE_FIXTURE: &str = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\n";
pub const LINES_1_TO_5: &str = "line 1\nline 2\nline 3\nline 4\nline 5\n";
pub const LINES_2_TO_4: &str = "line 2\nline 3\nline 4\n";
pub const LINES_18_TO_20: &str = "line 18\nline 19\nline 20\n";
pub struct RunningServer {
    pub client: reqwest::Client,
    pub base_url: String,
    pub mcp_url: String,
    pub kb: String,
    server_handle: JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct ListenerAddrs {
    http: std::net::SocketAddr,
}

impl RunningServer {
    async fn start(kb: String) -> Self {
        let listeners = ListenerAddrs {
            http: notedthat_api_http::testing::reserve_addr(),
        };
        let config = test_config(&kb, listeners);
        let backends = Backends {
            storage: Arc::new(InMemoryStorage::default()),
            store: Arc::new(InMemoryVectorStore::new()),
            embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        };
        let base_url = format!("http://{}", config.listen_addr);
        let mcp_url = format!("{base_url}/mcp");
        let server_handle = tokio::spawn(async move {
            notedthat_server::run::run_with(config, backends)
                .await
                .expect("server run failed");
        });

        wait_for_http(&format!("{base_url}/healthz"), SERVER_READY_TIMEOUT).await;

        Self {
            client: reqwest::Client::new(),
            base_url,
            mcp_url,
            kb,
            server_handle,
        }
    }

    pub async fn put_fixture(&self) {
        let response = self
            .client
            .put(format!(
                "{}/api/v1/knowledgebases/{}/hello.md",
                self.base_url, self.kb
            ))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Content-Type", "text/markdown")
            .body(TWENTY_LINE_FIXTURE)
            .send()
            .await
            .expect("PUT fixture failed");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    pub async fn get_hello_with_range(&self, range: &str) -> reqwest::Response {
        self.client
            .get(format!(
                "{}/api/v1/knowledgebases/{}/hello.md",
                self.base_url, self.kb
            ))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Range", range)
            .send()
            .await
            .expect("GET line range failed")
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

pub async fn fixture_server() -> RunningServer {
    let server = RunningServer::start(unique_kb()).await;
    server.put_fixture().await;
    server
}

pub fn assert_content_range_bytes(response: &reqwest::Response, expected: &str) {
    let header = response
        .headers()
        .get("x-content-range-bytes")
        .expect("X-Content-Range-Bytes header should be present")
        .to_str()
        .expect("X-Content-Range-Bytes should be valid ASCII");
    assert_eq!(header, expected);
}

/// Config for a server whose backends are injected.
///
/// The S3, Qdrant and embedder sections still have to be populated — `Config`
/// is the production type — but nothing reads them, because `run_with` never
/// builds a client from them. They point at unroutable placeholders so a
/// regression that *does* reach for them fails loudly.
fn test_config(kb: &str, listeners: ListenerAddrs) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(
        kb.to_string(),
        KbSlug::try_new(kb).expect("test KB slug is valid"),
    );

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: listeners.http,
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
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn unique_kb() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("notes-{nonce}")
}
