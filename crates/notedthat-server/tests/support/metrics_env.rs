//! A server with its metrics listener open, for the D69 suites.
//!
//! Built on `patch_backends`, which already assembles the real routers, the
//! real indexer worker and the real shutdown path over in-process backends.
//! What this adds is a second reserved port, the metrics switch, and
//! credentials chosen to be unmistakable in a text exposition: each is a
//! sentinel the label-leak suite greps for.

#![expect(dead_code, reason = "each suite uses a different part of this fixture")]

#[path = "patch_backends.rs"]
mod patch_backends;

use reqwest::StatusCode;
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long to wait for the server to bind after startup begins.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The service token this fixture's server accepts.
///
/// Distinctive on purpose: if this string turns up in an exposition, nothing
/// but a label could have put it there.
pub const LEAK_TOKEN: &str = "metrics-leak-canary-token";
/// `patch_backends` builds its `Config` with this; it is the same string, so
/// the token the server accepts is a sentinel from the moment it starts.
pub const API_TOKEN: &str = LEAK_TOKEN;
/// The `WebDAV` Basic username — the one caller-supplied principal name a
/// deployment without an identity provider has.
pub const LEAK_PRINCIPAL: &str = "metrics-leak-canary-principal";
/// Its password, which must never appear either.
pub const LEAK_PASSWORD: &str = "metrics-leak-canary-password";

/// Storage, vector store and embedder, all in-process.
pub fn in_memory_backends() -> notedthat_server::run::Backends {
    patch_backends::in_memory_backends()
}

pub struct MetricsServer {
    pub client: reqwest::Client,
    pub base_url: String,
    /// The metrics listener's base, whether or not anything is bound to it.
    pub metrics_base: String,
    pub kb: String,
    server_handle: JoinHandle<()>,
}

impl MetricsServer {
    /// A server whose metrics listener is open, over the in-process backends.
    pub async fn start() -> Self {
        Self::start_with(patch_backends::in_memory_backends(), true).await
    }

    /// A server over exactly `backends`, with or without a metrics listener.
    pub async fn start_with(backends: notedthat_server::run::Backends, metrics: bool) -> Self {
        let runtime = patch_backends::start_runtime_with_backends(16 * 1024 * 1024, backends);
        let mut config = runtime.config;
        config.webdav_username = LEAK_PRINCIPAL.to_string();
        config.webdav_password = LEAK_PASSWORD.to_string();

        // The test picks the port, as every other suite here does, so the
        // address is known before the server exists.
        let metrics_addr = notedthat_api_http::testing::reserve_addr();
        config.metrics_listen_addr = metrics.then_some(metrics_addr);

        let base_url = format!("http://{}", config.listen_addr);
        let metrics_base = format!("http://{metrics_addr}");
        let backends = runtime.backends;
        let server_handle = tokio::spawn(async move {
            notedthat_server::run::run_with(config, backends)
                .await
                .expect("server run failed");
        });

        wait_for_http(&format!("{base_url}/healthz"), SERVER_READY_TIMEOUT).await;

        Self {
            client: reqwest::Client::new(),
            base_url,
            metrics_base,
            kb: runtime.kb,
            server_handle,
        }
    }

    pub fn metrics_url(&self) -> String {
        format!("{}/metrics", self.metrics_base)
    }

    pub fn object_url(&self, path: &str) -> String {
        format!(
            "{}/api/v1/knowledgebases/{}/{}",
            self.base_url, self.kb, path
        )
    }

    /// Scrape the exposition, asserting it answered.
    pub async fn scrape(&self) -> String {
        let response = self
            .client
            .get(self.metrics_url())
            .send()
            .await
            .expect("the metrics listener should answer");
        assert_eq!(response.status(), StatusCode::OK);
        response.text().await.expect("the exposition should read")
    }

    pub async fn put_text(&self, path: &str, body: &str) -> reqwest::Response {
        self.client
            .put(self.object_url(path))
            .header("Authorization", format!("Bearer {LEAK_TOKEN}"))
            .header("Content-Type", "text/markdown")
            .body(body.to_owned())
            .send()
            .await
            .expect("PUT object failed")
    }

    pub async fn delete(&self, path: &str) -> reqwest::Response {
        self.client
            .delete(self.object_url(path))
            .header("Authorization", format!("Bearer {LEAK_TOKEN}"))
            .send()
            .await
            .expect("DELETE object failed")
    }

    pub async fn search(&self, query: &str) -> reqwest::Response {
        self.client
            .post(format!(
                "{}/api/v1/knowledgebases/{}/search",
                self.base_url, self.kb
            ))
            .header("Authorization", format!("Bearer {LEAK_TOKEN}"))
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .expect("search failed")
    }

    /// The per-knowledge-base index view, for waiting on the worker.
    pub async fn index_health(&self) -> serde_json::Value {
        self.client
            .get(format!(
                "{}/api/v1/knowledgebases/{}/index",
                self.base_url, self.kb
            ))
            .header("Authorization", format!("Bearer {LEAK_TOKEN}"))
            .send()
            .await
            .expect("index health failed")
            .json()
            .await
            .expect("index health should be JSON")
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

async fn wait_for_http(url: &str, timeout: Duration) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(response) = client.get(url).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "server did not become ready at {url} within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The exposition's series lines: neither blank nor a `#` comment.
pub fn series_lines(exposition: &str) -> Vec<&str> {
    exposition
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}
