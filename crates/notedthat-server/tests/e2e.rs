//! End-to-end tests for `notedthat-server` over in-process backends.
//!
//! The subject is the server itself — startup, the health and `llms.txt`
//! routes, bearer auth, and an object round trip — so storage, the vector store
//! and the embedder are substituted and handed to `run_with`. Everything else
//! is the real path.
//!
//! Run with: `cargo test -p notedthat-server --test e2e`
#![allow(missing_docs)]

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{KbSlug, TenantSlug};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_server::run::Backends;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Vector width the stub embedder and the provisioned collection agree on.
const EMBEDDING_DIM: u32 = 3;

/// Storage, vector store and embedder, all in-process.
fn in_memory_backends() -> Backends {
    Backends {
        storage: Arc::new(InMemoryStorage::default()),
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
    }
}

/// Config for a server whose backends are injected.
///
/// The S3, Qdrant and embedder sections still have to be populated — `Config`
/// is the production type — but nothing reads them, because `run_with` never
/// builds a client from them. They point at unroutable placeholders so a
/// regression that *does* reach for them fails loudly.
fn test_config(listen_addr: std::net::SocketAddr) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());
    Config {
        api_token: "e2e-test-token".to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:6334".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:9999".to_string(),
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

/// Poll `/healthz` until the server answers, or fail with a clear message.
///
/// Startup is in-process now, so this resolves in milliseconds; it replaces a
/// flat two-second sleep that was sized for container startup.
async fn wait_for_health(addr: std::net::SocketAddr) {
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/healthz");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not answer /healthz within 30s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn e2e_healthz_and_put_get() {
    let bound_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config(bound_addr);
    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    wait_for_health(bound_addr).await;

    let client = reqwest::Client::new();
    let base = format!("http://{bound_addr}");

    let resp = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let resp = client.get(format!("{base}/readyz")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let resp = client.get(format!("{base}/llms.txt")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; charset=utf-8"
    );

    let resp = client
        .get(format!("{base}/api/v1/knowledgebases"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let resp = client
        .put(format!("{base}/api/v1/knowledgebases/notes/hello.md"))
        .header("authorization", "Bearer e2e-test-token")
        .header("content-type", "text/markdown")
        .body("# Hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "/api/v1/knowledgebases/notes/hello.md"
    );

    let resp = client
        .get(format!("{base}/api/v1/knowledgebases/notes/hello.md"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "# Hello");

    server_handle.abort();
}

#[tokio::test]
async fn e2e_list_and_delete() {
    let bound_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config(bound_addr);
    let backends = in_memory_backends();
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });

    wait_for_health(bound_addr).await;

    let client = reqwest::Client::new();
    let base = format!("http://{bound_addr}");

    for name in &["file1.md", "file2.md"] {
        client
            .put(format!("{base}/api/v1/knowledgebases/notes/{name}"))
            .header("authorization", "Bearer e2e-test-token")
            .body("content")
            .send()
            .await
            .unwrap();
    }

    let resp = client
        .get(format!("{base}/api/v1/knowledgebases/notes"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    let count = json["objects"].as_array().map_or(0, Vec::len);
    assert!(count >= 2, "expected at least 2 objects, got {count}");

    let resp = client
        .delete(format!("{base}/api/v1/knowledgebases/notes/file1.md"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let resp = client
        .delete(format!("{base}/api/v1/knowledgebases/notes/file1.md"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    server_handle.abort();
}
