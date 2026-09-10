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
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const EMBEDDING_DIM: u32 = 4;
const TOKEN: &str = "fs-e2e-token";

struct Server {
    addr: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    _root: RootLock,
    dir: Option<tempfile::TempDir>,
    store_root: std::path::PathBuf,
    kb: String,
    /// The same store the server indexes into, for asserting on what was indexed.
    store: InMemoryVectorStore,
    slug: KbSlug,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Stop a server and start another over the same tree and the same index.
///
/// Everything a restart preserves is preserved: the directory, its contents, and what was
/// indexed before. Only the process-level state — the root lock, the watcher, the queue —
/// is rebuilt, which is what makes the startup comparison the only thing that could notice
/// a change made in between.
async fn restart(mut previous: Server, between: impl FnOnce(&Path)) -> Server {
    previous.handle.abort();
    let dir = previous
        .dir
        .take()
        .expect("the directory outlives one server");
    let store = previous.store.clone();
    let kb = previous.kb.clone();
    // Releases the root lock, so the next server can claim it.
    drop(previous);
    start_over(dir, Some((store, kb)), between).await
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
    start_seeded(|_| {}).await
}

/// Start a server over a tree that already has content, so the boot comparison has
/// something to find.
///
/// `seed` runs against the knowledge base's directory before the server starts, standing in
/// for whatever changed the tree while nothing was running.
async fn start_seeded(seed: impl FnOnce(&Path)) -> Server {
    start_over(tempfile::tempdir().expect("tempdir"), None, seed).await
}

/// Start a server over a given directory, optionally reusing an index a previous one built.
///
/// Reusing the index is what makes a restart observable: the tree and what is indexed both
/// carry over, so the startup comparison has the same two sides a real restart would.
async fn start_over(
    dir: tempfile::TempDir,
    existing: Option<(InMemoryVectorStore, String)>,
    seed: impl FnOnce(&Path),
) -> Server {
    let fs_config = FsConfig::new(dir.path().to_path_buf());
    let root = notedthat_storage_fs::open_root(&fs_config)
        .await
        .expect("storage root");
    let store_root = root.root().to_path_buf();

    let kb = existing.as_ref().map_or_else(
        || format!("notes-{}", std::process::id()),
        |(_, kb)| kb.clone(),
    );
    let slug = KbSlug::try_new(&kb).expect("slug");
    let addr = notedthat_api_http::testing::reserve_addr();

    let mut kbs = BTreeMap::new();
    kbs.insert(kb.clone(), slug.clone());

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

    let bucket_dir = store_root.join(format!("nt-default-{kb}"));
    std::fs::create_dir_all(&bucket_dir).expect("bucket directory");
    seed(&bucket_dir);

    // The real adapter, not a substitute — only Qdrant and the embedder are stood in for.
    let store = existing.map_or_else(InMemoryVectorStore::new, |(store, _)| store);
    let backends = Backends {
        storage: Arc::new(FsStorage::new(
            &fs_config,
            store_root.clone(),
            TenantSlug::default(),
        )),
        store: Arc::new(store.clone()),
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
        dir: Some(dir),
        store_root,
        slug,
        kb,
        store,
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

// ─── Changes made to the tree directly ──────────────────────────────────────

impl Server {
    /// Wait until `key` has index entries, or until `present` is satisfied.
    ///
    /// Polls rather than sleeps: a fast machine returns immediately and a slow one still
    /// passes, and the deadline only elapses when something is actually wrong.
    async fn wait_until(&self, what: &str, present: impl Fn(&BTreeSet<String>) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let keys = self.store.indexed_object_keys(&self.slug).await;
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

    async fn indexed(&self) -> BTreeSet<String> {
        self.store.indexed_object_keys(&self.slug).await
    }
}

/// The gap this whole feature closes: a file put into the tree by anything at all becomes
/// searchable, without ever being written through `NotedThat`.
#[tokio::test]
async fn a_file_written_into_the_tree_becomes_searchable() {
    let server = start().await;

    std::fs::write(
        server.bucket_dir().join("from-outside.md"),
        "# Outside\n\nWritten by something that is not NotedThat.",
    )
    .expect("write");

    server
        .wait_until("the new file to be indexed", |keys| {
            keys.contains("from-outside.md")
        })
        .await;
}

#[tokio::test]
async fn a_file_deleted_from_the_tree_stops_being_searchable() {
    let server = start().await;
    let path = server.bucket_dir().join("doomed.md");
    std::fs::write(&path, "# Doomed\n\nBody.").expect("write");
    server
        .wait_until("the file to be indexed", |keys| keys.contains("doomed.md"))
        .await;

    std::fs::remove_file(&path).expect("remove");

    server
        .wait_until("the file to be forgotten", |keys| {
            !keys.contains("doomed.md")
        })
        .await;
}

/// Renaming a folder is the operation a watcher alone gets wrong: the kernel reports the
/// two directories and none of the files under either, so both sides have to be compared.
#[tokio::test]
async fn renaming_a_folder_moves_its_files_in_search() {
    let server = start().await;
    std::fs::create_dir_all(server.bucket_dir().join("before")).expect("mkdir");
    std::fs::write(
        server.bucket_dir().join("before/note.md"),
        "# Note\n\nBody.",
    )
    .expect("write");
    server
        .wait_until("the file to be indexed", |keys| {
            keys.contains("before/note.md")
        })
        .await;

    std::fs::rename(
        server.bucket_dir().join("before"),
        server.bucket_dir().join("after"),
    )
    .expect("rename");

    server
        .wait_until("the rename to be reflected", |keys| {
            keys.contains("after/note.md") && !keys.contains("before/note.md")
        })
        .await;
}

/// Changes made while the server was not running raise no events at all, so the boot
/// comparison is the only thing that can find them.
#[tokio::test]
async fn a_tree_the_server_has_never_seen_is_indexed_at_startup() {
    let server = start_seeded(|bucket| {
        std::fs::create_dir_all(bucket.join("deep")).expect("mkdir");
        std::fs::write(bucket.join("existing.md"), "# Existing\n\nBody.").expect("write");
        std::fs::write(bucket.join("deep/nested.md"), "# Nested\n\nBody.").expect("write");
    })
    .await;

    server
        .wait_until("the pre-existing tree to be indexed", |keys| {
            keys.contains("existing.md") && keys.contains("deep/nested.md")
        })
        .await;
}

/// The half no walk can find on its own. A file deleted while the server was down leaves
/// nothing behind, so the only evidence is the index entry with no file under it.
#[tokio::test]
async fn a_file_deleted_while_the_server_was_down_is_forgotten_at_startup() {
    let server = start_seeded(|bucket| {
        std::fs::write(bucket.join("survivor.md"), "# Survivor\n\nBody.").expect("write");
    })
    .await;
    server
        .wait_until("the seeded tree to be indexed", |keys| {
            keys.contains("survivor.md")
        })
        .await;

    // Index an object, then take it away behind the server's back and start again over the
    // same store — exactly what a restart across a deletion looks like.
    std::fs::write(server.bucket_dir().join("removed.md"), "# Removed\n\nBody.").expect("write");
    server
        .wait_until("the second file to be indexed", |keys| {
            keys.contains("removed.md")
        })
        .await;

    let server = restart(server, |bucket| {
        std::fs::remove_file(bucket.join("removed.md")).expect("remove");
    })
    .await;

    server
        .wait_until("the deleted object to be forgotten", |keys| {
            !keys.contains("removed.md")
        })
        .await;
    assert!(
        server.indexed().await.contains("survivor.md"),
        "only the deleted object should be forgotten"
    );
}
