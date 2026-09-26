//! The `s3` backend's reconciliation pass (D67), end to end over in-process backends.
//!
//! The subject is the pass and its two triggers — startup, and
//! `POST …/index/reconcile` — so storage is the in-memory double under a config that
//! selects `s3` (the real adapter's listing is proven by the storage conformance suite
//! and the Docker e2e beside this file), the vector store is in-memory, and the embedder
//! is a stub. The server, the worker, the health view and the route are real.
//!
//! Every test here ends with `handle.abort()`, which drops the server future at an await
//! point and so **skips `serve`'s shutdown sequence** — `fs_watch.stop()`, then
//! `reconciler.stop()`, then the indexer drain. That is a deliberate gap, not an
//! oversight: `run_with` takes no shutdown handle, and `serve` is driven only by SIGTERM
//! or SIGINT, which a test cannot raise without signalling the whole test binary. What
//! the sequence is for is covered next to the code instead — `Reconciler::stop()`
//! cancelling a pass that is blocked on a full queue is pinned by
//! `stop_returns_while_a_pass_is_blocked_on_a_full_queue` in `run/reconcile.rs`, and its
//! position before the drain by the comments in `serve`. Giving `run_with` a shutdown
//! handle would close the gap properly and is worth doing when something else needs one.
//!
//! Run with: `cargo test -p notedthat-server --test s3_reconcile_e2e`
#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, TenantSlug};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{
    Config, EmbedderConfig, LogFormat, ServerQdrantConfig, StorageConfig,
};
use notedthat_server::run::Backends;

const TOKEN: &str = "s3-reconcile-token";
const KB: &str = "notes";
const EMBEDDING_DIM: u32 = 3;

struct Server {
    addr: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    storage: Arc<InMemoryStorage>,
    store: InMemoryVectorStore,
    slug: KbSlug,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn indexed(&self) -> BTreeSet<String> {
        self.store.indexed_object_keys(&self.slug).await
    }

    /// Wait, bounded, until the index holds what `present` asks for — a real signal,
    /// never a fixed sleep.
    async fn wait_until(&self, what: &str, present: impl Fn(&BTreeSet<String>) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let keys = self.indexed().await;
            if present(&keys) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}; indexed keys are {keys:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// `GET …/index` as the service token, decoded.
    async fn index_health(&self) -> serde_json::Value {
        reqwest::Client::new()
            .get(self.url(&format!("/api/v1/knowledgebases/{KB}/index")))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("index")
            .json()
            .await
            .expect("json")
    }

    /// Wait, bounded, until `/index` reports a completed pass no older than `after` and
    /// not the same one.
    ///
    /// `at` is serialized to second resolution, so two passes over an unchanged bucket
    /// inside the same second are byte-identical and cannot be told apart by timestamp.
    /// The condition therefore asks for both: an `at` that has not gone backwards, and a
    /// record that differs from the one the caller already saw. A caller that expects two
    /// consecutive identical passes cannot use this helper and should wait on the counts.
    async fn wait_reconciled(&self, after: Option<&serde_json::Value>) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let health = self.index_health().await;
            let last = &health["last_reconcile"];
            let moved = match after {
                None => !last.is_null(),
                // RFC 3339 at second resolution, so the strings order the same way the
                // instants do.
                Some(previous) => {
                    !last.is_null()
                        && last["at"].as_str() >= previous["at"].as_str()
                        && last != previous
                }
            };
            if moved {
                return last.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for a reconciliation pass; /index is {health}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn post_reconcile(&self, token: Option<&str>, kb: &str) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(self.url(&format!("/api/v1/knowledgebases/{kb}/index/reconcile")));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.expect("post")
    }

    async fn put_out_of_band(&self, key: &str, body: &str) {
        self.storage
            .put_object(
                &self.slug,
                &ObjectPath::try_from(key).unwrap(),
                Bytes::from(body.to_string()),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("put");
    }

    async fn delete_out_of_band(&self, key: &str) {
        self.storage
            .delete_object(
                &self.slug,
                &ObjectPath::try_from(key).unwrap(),
                ConditionalHeaders::default(),
            )
            .await
            .expect("delete");
    }
}

fn config(listen_addr: std::net::SocketAddr, reconcile_on_startup: bool) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(KB.to_string(), KbSlug::try_new(KB).unwrap());
    let StorageConfig::S3(mut s3) = notedthat_server::config::unroutable_storage_placeholder()
    else {
        panic!("the placeholder selects s3")
    };
    s3.reconcile_on_startup = reconcile_on_startup;
    Config {
        api_token: TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr,
        metrics_listen_addr: None,
        storage: StorageConfig::S3(s3),
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
        webdav_username: "s3-e2e-user".to_string(),
        webdav_password: "s3-e2e-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        mcp_anonymous: notedthat_server::config::McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        ready_probe_interval_ms: 5_000,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        mcp_max_sessions: notedthat_mcp::DEFAULT_MAX_SESSIONS,
        request_bounds: notedthat_server::config::RequestBoundsConfig::default(),
        staging: notedthat_core::StagingConfig::default(),
        oidc: None,
    }
}

/// Boot a server over a bucket that already holds `seed` — objects written by "someone
/// else", before `NotedThat` ever saw them.
async fn start(seed: &[(&str, &str)], reconcile_on_startup: bool) -> Server {
    let slug = KbSlug::try_new(KB).unwrap();
    let storage = Arc::new(InMemoryStorage::with_kbs([&slug]));
    for (key, body) in seed {
        storage
            .put_object(
                &slug,
                &ObjectPath::try_from(*key).unwrap(),
                Bytes::from((*body).to_string()),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed");
    }
    let store = InMemoryVectorStore::new();
    let addr = notedthat_api_http::testing::reserve_addr();
    let config = config(addr, reconcile_on_startup);
    let backends = Backends {
        storage: storage.clone(),
        store: Arc::new(store.clone()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        events: None,
    };
    let handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });
    wait_for_health(addr).await;
    Server {
        addr,
        handle,
        storage,
        store,
        slug,
    }
}

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

fn counts(last: &serde_json::Value) -> (u64, u64, u64, u64) {
    (
        last["objects_on_disk"].as_u64().unwrap(),
        last["unchanged"].as_u64().unwrap(),
        last["changed"].as_u64().unwrap(),
        last["orphaned"].as_u64().unwrap(),
    )
}

/// Objects that were in the bucket before the server started are found by the startup
/// pass, indexed, and reported — the deployment learns about a bucket it did not fill.
#[tokio::test]
async fn the_startup_pass_indexes_what_the_bucket_already_held() {
    let server = start(
        &[("a.md", "# A\n\nAlpha."), ("b.md", "# B\n\nBravo.")],
        true,
    )
    .await;

    server
        .wait_until("the seeded objects to be indexed", |keys| {
            keys.contains("a.md") && keys.contains("b.md")
        })
        .await;
    let last = server.wait_reconciled(None).await;
    assert_eq!(counts(&last), (2, 0, 2, 0), "{last}");
    assert!(last["scope"].is_null(), "a whole-base pass has no scope");

    // Once the queue drains the base is healthy, with the pass on record.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let health = server.index_health().await;
        if health["state"] == "healthy" {
            assert_eq!(health["last_reconcile"], last);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{health}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.handle.abort();
}

/// An object deleted behind the server's back is forgotten by the next requested pass;
/// one written behind its back is indexed; one untouched costs nothing.
#[tokio::test]
async fn a_requested_pass_follows_out_of_band_changes() {
    let server = start(
        &[
            ("keep.md", "# Keep\n\nStays."),
            ("gone.md", "# Gone\n\nGoes."),
        ],
        true,
    )
    .await;
    server
        .wait_until("the seed to be indexed", |keys| {
            keys.contains("keep.md") && keys.contains("gone.md")
        })
        .await;
    let startup = server.wait_reconciled(None).await;

    server.delete_out_of_band("gone.md").await;
    server.put_out_of_band("new.md", "# New\n\nArrived.").await;

    let response = server.post_reconcile(Some(TOKEN), KB).await;
    assert_eq!(response.status(), 202, "{}", response.text().await.unwrap());

    server
        .wait_until(
            "the deleted object to be forgotten and the new one indexed",
            |keys| !keys.contains("gone.md") && keys.contains("new.md") && keys.contains("keep.md"),
        )
        .await;
    let last = server.wait_reconciled(Some(&startup)).await;
    assert_eq!(counts(&last), (2, 1, 1, 1), "{last}");

    // Nothing changed since: the next pass reads no object and reports it.
    let response = server.post_reconcile(Some(TOKEN), KB).await;
    assert_eq!(response.status(), 202);
    let clean = server.wait_reconciled(Some(&last)).await;
    assert_eq!(counts(&clean), (2, 2, 0, 0), "{clean}");
    server.handle.abort();
}

/// With the switch off, nothing runs at startup — the base is `healthy` with no pass on
/// record — and the operator's request still works.
#[tokio::test]
async fn with_the_switch_off_only_a_request_runs_a_pass() {
    let server = start(&[("a.md", "# A\n\nAlpha.")], false).await;

    let health = server.index_health().await;
    assert!(health["last_reconcile"].is_null(), "{health}");
    assert_eq!(health["state"], "healthy", "{health}");
    assert!(server.indexed().await.is_empty());

    let response = server.post_reconcile(Some(TOKEN), KB).await;
    assert_eq!(response.status(), 202);
    server
        .wait_until("the requested pass to index the object", |keys| {
            keys.contains("a.md")
        })
        .await;
    assert_eq!(counts(&server.wait_reconciled(None).await), (1, 0, 1, 0));
    server.handle.abort();
}

/// The route's refusals over the real middleware and router.
#[tokio::test]
async fn the_route_is_the_operators_alone() {
    let server = start(&[], false).await;

    let response = server.post_reconcile(None, KB).await;
    assert_eq!(response.status(), 401, "no credential");
    let response = server.post_reconcile(Some("not-the-token"), KB).await;
    assert_eq!(response.status(), 401, "a credential that does not verify");
    let response = server.post_reconcile(Some(TOKEN), "undeclared").await;
    assert_eq!(response.status(), 404, "an undeclared knowledge base");
    server.handle.abort();
}
