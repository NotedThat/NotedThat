//! The real server, over the real filesystem backend.
//!
//! Everything but the vector store and the embedder is the production path: startup,
//! provisioning, the API and `WebDAV` routers, and `FsStorage` writing to a temporary
//! directory. What this proves that the adapter's own tests cannot is that the surfaces
//! above the `Storage` seam behave the same on this backend — and that the store they
//! leave behind is the browsable tree the feature promises.
//!
//! Run with: `cargo test -p notedthat-server --test fs_backend_e2e`
#![allow(missing_docs)]

use notedthat_core::{KbSlug, TenantSlug};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{
    Config, EmbedderConfig, LogFormat, ServerQdrantConfig, StorageConfig,
};
use notedthat_server::run::Backends;
use notedthat_storage_fs::{FsConfig, FsStorage, RootLock};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const EMBEDDING_DIM: u32 = 4;
const TOKEN: &str = "fs-e2e-token";

struct Server {
    addr: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    _root: RootLock,
    _dir: tempfile::TempDir,
    store_root: std::path::PathBuf,
    kb: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The directory holding this knowledge base's objects.
    fn bucket_dir(&self) -> std::path::PathBuf {
        self.store_root.join(format!("nt-default-{}", self.kb))
    }
}

async fn start() -> Server {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs_config = FsConfig::new(dir.path().to_path_buf());
    let root = notedthat_storage_fs::open_root(&fs_config)
        .await
        .expect("storage root");
    let store_root = root.root().to_path_buf();

    let kb = format!("notes-{}", std::process::id());
    let slug = KbSlug::try_new(&kb).expect("slug");
    let addr = notedthat_api_http::testing::reserve_addr();

    let mut kbs = BTreeMap::new();
    kbs.insert(kb.clone(), slug);

    let config = Config {
        api_token: TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: addr,
        storage: StorageConfig::Fs(fs_config.clone()),
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
        webdav_username: "fs-e2e-user".to_string(),
        webdav_password: "fs-e2e-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        max_patchable_size: 10 * 1024 * 1024,
        staging: notedthat_core::StagingConfig::default(),
    };

    // The real adapter, not a substitute — only Qdrant and the embedder are stood in for.
    let backends = Backends {
        storage: Arc::new(FsStorage::new(
            &fs_config,
            store_root.clone(),
            TenantSlug::default(),
        )),
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
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
        _root: root,
        _dir: dir,
        store_root,
        kb,
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

fn object_url(server: &Server, key: &str) -> String {
    server.url(&format!("/api/v1/knowledgebases/{}/{key}", server.kb))
}

/// Startup provisions the KB, and a write through the API lands as a file at its key
/// path — the whole point of the backend, observed from outside.
#[tokio::test]
async fn a_write_through_the_api_lands_as_a_browsable_file() {
    let server = start().await;
    let client = reqwest::Client::new();

    // Provisioning wrote the manifest as an ordinary file.
    assert!(
        server
            .bucket_dir()
            .join(".notedthat/manifest.json")
            .is_file(),
        "startup should have provisioned the knowledge base directory"
    );

    let response = client
        .put(object_url(&server, "notes/hello.md"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "text/markdown")
        .body("# Hello\n")
        .send()
        .await
        .expect("put");
    assert_eq!(response.status(), 201, "PUT should create the object");

    let on_disk = server.bucket_dir().join("notes/hello.md");
    assert_eq!(
        std::fs::read_to_string(&on_disk).expect("the object should be a plain file"),
        "# Hello\n"
    );

    let read = client
        .get(object_url(&server, "notes/hello.md"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("get");
    assert_eq!(read.status(), 200);
    assert_eq!(
        read.headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/markdown")
    );
    assert_eq!(read.text().await.expect("body"), "# Hello\n");
}

/// A file edited in place by a person is served with its new bytes and a new `ETag`.
/// This is the behaviour that makes a browsable store honest rather than a trap.
#[tokio::test]
async fn an_edit_made_in_the_tree_is_served_with_a_new_etag() {
    let server = start().await;
    let client = reqwest::Client::new();

    client
        .put(object_url(&server, "hello.md"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "text/markdown")
        .body("first")
        .send()
        .await
        .expect("put");

    let first_etag = client
        .head(object_url(&server, "hello.md"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("head")
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .expect("an ETag");

    std::fs::write(server.bucket_dir().join("hello.md"), b"edited in place").expect("edit");

    let read = client
        .get(object_url(&server, "hello.md"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("get");
    let second_etag = read
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .expect("an ETag");

    assert_eq!(read.text().await.expect("body"), "edited in place");
    assert_ne!(
        first_etag, second_etag,
        "an out-of-band edit must change the validator, or clients cache stale content"
    );
}

/// Optimistic concurrency across the seam: the header the client sends is enforced by
/// the adapter, and reaches the client as the status D43 specifies.
#[tokio::test]
async fn conditional_requests_round_trip_through_the_api() {
    let server = start().await;
    let client = reqwest::Client::new();

    let created = client
        .put(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "text/markdown")
        .header("If-None-Match", "*")
        .body("one")
        .send()
        .await
        .expect("create-only put");
    assert_eq!(created.status(), 201);

    let clobbered = client
        .put(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .header("If-None-Match", "*")
        .body("two")
        .send()
        .await
        .expect("second create-only put");
    assert_eq!(
        clobbered.status(),
        412,
        "If-None-Match: * must stay create-only"
    );

    let etag = client
        .head(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("head")
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .expect("an ETag");

    let stale = client
        .put(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .header("If-Match", "\"stale\"")
        .body("three")
        .send()
        .await
        .expect("stale put");
    assert_eq!(stale.status(), 412);

    let current = client
        .put(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .header("If-Match", &etag)
        .body("three")
        .send()
        .await
        .expect("current put");
    assert!(current.status().is_success());

    // The successful write above advanced the validator, so re-read it.
    let current_etag = client
        .head(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("head")
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .expect("an ETag");
    assert_ne!(current_etag, etag, "a write must change the validator");

    let not_modified = client
        .get(object_url(&server, "a.md"))
        .bearer_auth(TOKEN)
        .header("If-None-Match", &current_etag)
        .send()
        .await
        .expect("conditional get");
    assert_eq!(
        not_modified.status(),
        304,
        "a matching If-None-Match on GET is a 304"
    );
}

#[tokio::test]
async fn range_reads_and_listing_work_over_the_api() {
    let server = start().await;
    let client = reqwest::Client::new();

    for name in ["one.md", "two.md", "nested/three.md"] {
        client
            .put(object_url(&server, name))
            .bearer_auth(TOKEN)
            .header("Content-Type", "text/markdown")
            .body("0123456789")
            .send()
            .await
            .expect("put");
    }

    let ranged = client
        .get(object_url(&server, "one.md"))
        .bearer_auth(TOKEN)
        .header("Range", "bytes=2-5")
        .send()
        .await
        .expect("range get");
    assert_eq!(ranged.status(), 206);
    assert_eq!(
        ranged
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok()),
        Some("bytes 2-5/10")
    );
    assert_eq!(ranged.text().await.expect("body"), "2345");

    let listed: serde_json::Value = client
        .get(server.url(&format!("/api/v1/knowledgebases/{}", server.kb)))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let keys: Vec<&str> = listed["objects"]
        .as_array()
        .expect("objects array")
        .iter()
        .filter_map(|object| object["key"].as_str())
        .collect();
    assert!(keys.contains(&"one.md"), "{keys:?}");
    assert!(keys.contains(&"nested/three.md"), "{keys:?}");
}

/// Deleting through the API leaves no empty directories behind, so the tree a person
/// browses matches the key space the API reports.
#[tokio::test]
async fn deleting_through_the_api_tidies_the_tree() {
    let server = start().await;
    let client = reqwest::Client::new();

    client
        .put(object_url(&server, "deep/nested/note.md"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "text/markdown")
        .body("x")
        .send()
        .await
        .expect("put");
    assert!(server.bucket_dir().join("deep/nested/note.md").is_file());

    let deleted = client
        .delete(object_url(&server, "deep/nested/note.md"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 204);

    assert!(
        !server.bucket_dir().join("deep").exists(),
        "emptied directories should not linger in a tree people browse"
    );
}

/// The store is browsable, which means nothing but objects may appear inside a knowledge
/// base directory — no sidecars, no lock file, no temp leftovers.
#[tokio::test]
async fn the_knowledge_base_directory_contains_only_objects() {
    let server = start().await;
    let client = reqwest::Client::new();

    client
        .put(object_url(&server, "note.md"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "text/markdown")
        .body("x")
        .send()
        .await
        .expect("put");

    let mut names = entries(&server.bucket_dir());
    names.sort();
    assert_eq!(names, vec![".notedthat".to_string(), "note.md".to_string()]);

    // Metadata and the process lock live above the knowledge bases, not inside them.
    assert!(server.store_root.join(".notedthat-meta").is_dir());
    assert!(server.store_root.join(".notedthat.lock").is_file());
}

fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}
