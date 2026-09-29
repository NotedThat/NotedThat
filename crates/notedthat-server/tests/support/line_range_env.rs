use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::KbSlug;
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig};
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
            events: None,
        };
        let base_url = format!("http://{}", config.listen_addr);
        let server_handle = tokio::spawn(async move {
            notedthat_server::run::run_with(config, backends)
                .await
                .expect("server run failed");
        });

        wait_for_http(&format!("{base_url}/healthz"), SERVER_READY_TIMEOUT).await;

        Self {
            client: reqwest::Client::new(),
            base_url,
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

/// Config for a server whose backends are injected; the rest is
/// [`Config::for_tests`], whose backend addresses are unroutable.
fn test_config(kb: &str, listeners: ListenerAddrs) -> Config {
    let base = Config::for_tests();
    Config {
        api_token: API_TOKEN.to_string(),
        kbs: BTreeMap::from([(
            kb.to_string(),
            KbSlug::try_new(kb).expect("test KB slug is valid"),
        )]),
        listen_addr: listeners.http,
        embedder: EmbedderConfig {
            dimensions: EMBEDDING_DIM,
            ..base.embedder
        },
        ..base
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
