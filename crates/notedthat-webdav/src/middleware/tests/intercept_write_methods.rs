use super::super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request as HttpRequest,
    middleware::from_fn_with_state,
    routing::any,
};
use bytes::Bytes;
use notedthat_core::testing::{
    InMemoryStorage, ScriptedStorage, StorageCall, StorageOp, compute_etag,
};
use notedthat_core::{CopyObjectOptions, KbSlug, ObjectPath, ObjectRead, Storage};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;
use tower::util::ServiceExt;

/// A store with both declared knowledge bases provisioned and nothing in them.
fn empty_storage() -> ScriptedStorage {
    ScriptedStorage::with_kbs([&kb_slug("notes"), &kb_slug("scratch")])
}

/// Seeding, inspection and race shorthands over the store these tests drive.
trait Fixture {
    /// Store a Markdown object with a pinned `ETag`.
    async fn insert(&self, kb: &str, path: &str, bytes: impl Into<Bytes>, etag: &str);
    async fn insert_with_content_type(
        &self,
        kb: &str,
        path: &str,
        bytes: impl Into<Bytes>,
        etag: &str,
        content_type: &str,
    );
    async fn get_stored(&self, kb: &str, path: &str) -> Option<ObjectRead>;
    /// Whether any write of a request body reached the store.
    fn put_called(&self) -> bool;
    fn copy_options(&self) -> Vec<CopyObjectOptions>;
    fn staged_paths(&self) -> Vec<std::path::PathBuf>;
    fn staged_lengths(&self) -> Vec<u64>;
    /// A concurrent writer creates the copy's destination just before the copy lands.
    fn race_destination_before_copy(&self, kb: &str, path: &str);
    /// A concurrent writer changes the copy's source just before the copy lands.
    fn change_source_before_copy(&self, kb: &str, path: &str);
    /// A concurrent writer changes the copy's source just after the copy lands.
    fn change_source_after_copy(&self, kb: &str, path: &str);
}

impl Fixture for ScriptedStorage {
    async fn insert(&self, kb: &str, path: &str, bytes: impl Into<Bytes>, etag: &str) {
        self.insert_with_content_type(kb, path, bytes, etag, "text/markdown")
            .await;
    }

    async fn insert_with_content_type(
        &self,
        kb: &str,
        path: &str,
        bytes: impl Into<Bytes>,
        etag: &str,
        content_type: &str,
    ) {
        self.inner()
            .seed(kb, path, bytes, Some(content_type), Some(etag))
            .await;
    }

    async fn get_stored(&self, kb: &str, path: &str) -> Option<ObjectRead> {
        self.inner().object(kb, path).await
    }

    fn put_called(&self) -> bool {
        self.count(StorageOp::PutObject) + self.count(StorageOp::PutStagedObject) > 0
    }

    fn copy_options(&self) -> Vec<CopyObjectOptions> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                StorageCall::CopyObject { options, .. } => Some(options),
                _ => None,
            })
            .collect()
    }

    fn staged_paths(&self) -> Vec<std::path::PathBuf> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                StorageCall::PutStagedObject { staged_file, .. } => staged_file,
                _ => None,
            })
            .collect()
    }

    fn staged_lengths(&self) -> Vec<u64> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                StorageCall::PutStagedObject { len, .. } => Some(len),
                _ => None,
            })
            .collect()
    }

    fn race_destination_before_copy(&self, kb: &str, path: &str) {
        let (kb, path) = (kb.to_string(), path.to_string());
        self.before(StorageOp::CopyObject, move |store| {
            let (kb, path) = (kb.clone(), path.clone());
            async move {
                store
                    .seed(
                        &kb,
                        &path,
                        "racing writer",
                        Some("text/plain"),
                        Some("\"race\""),
                    )
                    .await;
            }
        });
    }

    fn change_source_before_copy(&self, kb: &str, path: &str) {
        let (kb, path) = (kb.to_string(), path.to_string());
        self.before(StorageOp::CopyObject, move |store| {
            retag(store, kb.clone(), path.clone(), "\"changed\"")
        });
    }

    fn change_source_after_copy(&self, kb: &str, path: &str) {
        let (kb, path) = (kb.to_string(), path.to_string());
        self.after(StorageOp::CopyObject, move |store| {
            retag(store, kb.clone(), path.clone(), "\"changed\"")
        });
    }
}

/// Give the object at `path` a new `ETag`, keeping its bytes and content type.
async fn retag(store: InMemoryStorage, kb: String, path: String, etag: &'static str) {
    if let Some(current) = store.object(&kb, &path).await {
        store
            .seed(
                &kb,
                &path,
                current.bytes,
                current.meta.content_type.as_deref(),
                Some(etag),
            )
            .await;
    }
}

fn kb_slug(value: &str) -> KbSlug {
    KbSlug::try_new(value).expect("valid KB slug")
}

fn declared_kbs(values: &[&str]) -> BTreeMap<String, KbSlug> {
    values
        .iter()
        .map(|value| ((*value).to_string(), kb_slug(value)))
        .collect()
}

fn test_state(storage: ScriptedStorage) -> WebDavState {
    let (indexer_tx, _rx) = mpsc::channel(1024);
    test_state_with_indexer_tx(storage, indexer_tx)
}

#[allow(clippy::needless_pass_by_value)]
fn test_state_with_indexer_tx(
    storage: ScriptedStorage,
    indexer_tx: mpsc::Sender<notedthat_indexer::IndexEvent>,
) -> WebDavState {
    let storage: Arc<dyn Storage> = Arc::new(storage);
    WebDavState {
        authenticator: Arc::new(
            notedthat_core::Authenticator::new("test-service-token")
                .with_basic("user".to_string(), "pass".to_string()),
        ),
        storage,
        staging_config: notedthat_core::StagingConfig::default(),
        declared_kbs: Arc::new(declared_kbs(&["notes", "scratch"])),
        access_policies: Arc::new(BTreeMap::new()),
        indexer_tx: (&indexer_tx).into(),
        events: None,
        index_health: Arc::new(notedthat_indexer::IndexHealth::new()),
    }
}

fn app(storage: ScriptedStorage) -> Router {
    Router::new()
        .fallback(any(|| async { "inner handler reached" }))
        .layer(from_fn_with_state(
            test_state(storage),
            intercept_write_methods,
        ))
}

fn app_with_indexer_tx(
    storage: ScriptedStorage,
    indexer_tx: mpsc::Sender<notedthat_indexer::IndexEvent>,
) -> Router {
    Router::new()
        .fallback(any(|| async { "inner handler reached" }))
        .layer(from_fn_with_state(
            test_state_with_indexer_tx(storage, indexer_tx),
            intercept_write_methods,
        ))
}

fn app_with_staging_config(
    storage: ScriptedStorage,
    staging_config: notedthat_core::StagingConfig,
) -> Router {
    let mut state = test_state(storage);
    state.staging_config = staging_config;
    Router::new()
        .fallback(any(|| async { "inner handler reached" }))
        .layer(from_fn_with_state(state, intercept_write_methods))
}

fn object_path(value: &str) -> ObjectPath {
    ObjectPath::try_from(value).expect("valid object path")
}

async fn response_body(resp: Response) -> String {
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

mod mutation_safety;

#[tokio::test]
async fn test_get_passes_through() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(response_body(resp).await, "inner handler reached");
}

#[tokio::test]
async fn test_put_creates_and_returns_201_etag() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/new.md")
                .header("content-type", "text/markdown")
                .body(Body::from("# New"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("etag").unwrap(),
        compute_etag(b"# New").as_str()
    );
    // No pre-write HEAD: the write itself reports that it created the object.
    assert_eq!(storage.ops(), vec![StorageOp::PutStagedObject]);
}

#[tokio::test]
async fn test_put_overwrite_returns_204() {
    let storage = empty_storage();
    storage
        .insert("notes", "old.md", Bytes::from_static(b"old"), "\"old\"")
        .await;
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/old.md")
                .body(Body::from("new"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

fn proppatch(uri: &str, headers: &[(&str, &str)], body: &'static str) -> HttpRequest<Body> {
    let mut builder = HttpRequest::builder().method("PROPPATCH").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body)).unwrap()
}

const DISPLAYNAME: &str = r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><D:displayname>x</D:displayname></D:prop></D:set></D:propertyupdate>"#;

/// RFC 4918 §9.2: PROPPATCH answers 207 per property and stores nothing.
#[tokio::test]
async fn test_proppatch_refuses_protected_properties_with_207() {
    let storage = empty_storage();
    storage
        .insert("notes", "a.md", Bytes::from_static(b"body"), "\"a\"")
        .await;
    let resp = app(storage.clone())
        .oneshot(proppatch("/webdav/notes/a.md", &[], DISPLAYNAME))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
    let body = response_body(resp).await;
    assert!(
        body.contains("<D:href>/webdav/notes/a.md</D:href>"),
        "{body}"
    );
    assert!(body.contains("HTTP/1.1 403 Forbidden"), "{body}");
    assert!(
        body.contains("<D:cannot-modify-protected-property/>"),
        "{body}"
    );
    assert_eq!(storage.ops(), vec![StorageOp::HeadObject]);
}

#[tokio::test]
async fn test_proppatch_on_a_knowledge_base_root_returns_207() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(proppatch("/webdav/notes/", &[], DISPLAYNAME))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
}

#[tokio::test]
async fn test_proppatch_honours_if_and_if_match() {
    let storage = empty_storage();
    storage
        .insert("notes", "a.md", Bytes::from_static(b"body"), "\"a\"")
        .await;
    for headers in [
        &[("if-match", "\"stale\"")][..],
        &[("if", "([\"stale\"])")],
        &[("if", "(<opaquelocktoken:x>)")],
    ] {
        let resp = app(storage.clone())
            .oneshot(proppatch("/webdav/notes/a.md", headers, DISPLAYNAME))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PRECONDITION_FAILED,
            "{headers:?}"
        );
    }
    let resp = app(storage)
        .oneshot(proppatch(
            "/webdav/notes/a.md",
            &[("if", "([\"a\"])")],
            DISPLAYNAME,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
}

#[tokio::test]
async fn test_proppatch_with_a_malformed_body_returns_400() {
    let storage = empty_storage();
    storage
        .insert("notes", "a.md", Bytes::from_static(b"body"), "\"a\"")
        .await;
    let resp = app(storage)
        .oneshot(proppatch("/webdav/notes/a.md", &[], "<D:oops"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_put_with_if_match_wrong_etag_returns_412() {
    let storage = empty_storage();
    storage
        .insert("notes", "old.md", Bytes::from_static(b"old"), "\"old\"")
        .await;
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/old.md")
                .header("if-match", "\"wrong\"")
                .body(Body::from("new"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn test_put_content_length_over_5gib_returns_413_before_reading_body() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/huge.md")
                .header(
                    "content-length",
                    (notedthat_write::MAX_UPLOAD_BYTES + 1).to_string(),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(storage.ops().is_empty());
}

#[tokio::test]
async fn test_put_md_with_octet_stream_stored_as_text_markdown() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/sniff.md")
                .header("content-type", "application/octet-stream")
                .body(Body::from("# Markdown"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let stored = storage.get_stored("notes", "sniff.md").await.unwrap();
    assert_eq!(stored.meta.content_type.as_deref(), Some("text/markdown"));
}

#[tokio::test]
async fn test_put_to_non_declared_kb_returns_403() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/unknown/file.md")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_put_returns_503_with_retry_after_when_indexer_backpressure() {
    let storage = empty_storage();
    let (indexer_tx, _rx) = mpsc::channel(1);
    indexer_tx
        .try_send(notedthat_indexer::IndexEvent::Upsert {
            kb: kb_slug("notes"),
            object_key: object_path("queued.md"),
            etag: "\"queued\"".to_string(),
            mtime: 0,
        })
        .expect("queue accepts the prefilled event");

    let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/x.md")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    let body = response_body(resp).await;
    assert!(body.contains("backend_unavailable"));
    assert!(body.contains("object stored"));
    assert!(storage.get_stored("notes", "x.md").await.is_some());
}

#[tokio::test]
async fn test_delete_idempotent_returns_204() {
    let storage = empty_storage();
    let first = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("DELETE")
                .uri("/webdav/notes/missing.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let second = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("DELETE")
                .uri("/webdav/notes/missing.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    assert_eq!(second.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_delete_with_if_match_wrong_etag_returns_412() {
    let storage = empty_storage();
    storage
        .insert("notes", "delete.md", Bytes::from_static(b"old"), "\"old\"")
        .await;
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("DELETE")
                .uri("/webdav/notes/delete.md")
                .header("if-match", "\"wrong\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn test_delete_returns_503_with_retry_after_when_indexer_backpressure() {
    let storage = empty_storage();
    storage
        .insert("notes", "y.md", Bytes::from_static(b"y"), "\"old\"")
        .await;
    let (indexer_tx, _rx) = mpsc::channel(1);
    indexer_tx
        .try_send(notedthat_indexer::IndexEvent::Tombstone {
            kb: kb_slug("notes"),
            object_key: object_path("queued.md"),
        })
        .expect("queue accepts the prefilled event");

    let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
        .oneshot(
            HttpRequest::builder()
                .method("DELETE")
                .uri("/webdav/notes/y.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    let body = response_body(resp).await;
    assert!(body.contains("backend_unavailable"));
    assert!(body.contains("deleted from storage; retry to clear from search index"));
    assert!(!body.contains("object stored; indexer queue full — retry to re-enqueue"));
    assert!(storage.get_stored("notes", "y.md").await.is_none());
}

#[tokio::test]
async fn test_move_missing_destination_returns_400() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/source.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_move_cross_server_returns_502_destination_different_server() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/source.md")
                .header("host", "example.test")
                .header("destination", "http://other.test/notes/dest.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert!(
        response_body(resp)
            .await
            .contains("destination-different-server")
    );
}

#[tokio::test]
async fn test_move_cross_kb_returns_403_cannot_modify_source() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/source.md")
                .header("destination", "/webdav/scratch/dest.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(response_body(resp).await.contains("cannot-modify-source"));
}

#[tokio::test]
async fn test_move_single_object_returns_201_and_calls_commit_then_commit_delete() {
    let storage = empty_storage();
    storage
        .insert(
            "notes",
            "source.md",
            Bytes::from_static(b"source"),
            "\"source\"",
        )
        .await;
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/source.md")
                .header("destination", "/webdav/notes/dest.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        storage.ops(),
        vec![
            StorageOp::HeadObject,
            StorageOp::CopyObject,
            StorageOp::DeleteObject
        ]
    );
    assert!(storage.get_stored("notes", "source.md").await.is_none());
    assert!(storage.get_stored("notes", "dest.md").await.is_some());
}

#[tokio::test]
async fn test_copy_single_object_returns_201_and_calls_only_commit() {
    let storage = empty_storage();
    storage
        .insert(
            "notes",
            "source.md",
            Bytes::from_static(b"source"),
            "\"source\"",
        )
        .await;
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/source.md")
                .header("destination", "/webdav/notes/copy.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers().get("etag").unwrap(), "\"source\"");
    assert_eq!(
        storage.ops(),
        vec![StorageOp::HeadObject, StorageOp::CopyObject]
    );
    assert!(storage.get_stored("notes", "source.md").await.is_some());
    assert!(storage.get_stored("notes", "copy.md").await.is_some());
    assert!(!storage.ops().contains(&StorageOp::GetObject));
    assert_eq!(storage.copy_options()[0].destination_if_none_match, None);
}

#[tokio::test]
async fn test_copy_or_move_maps_destination_indexer_backpressure_to_503() {
    let storage = empty_storage();
    storage
        .insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"")
        .await;
    let (indexer_tx, _rx) = mpsc::channel(1);
    indexer_tx
        .try_send(notedthat_indexer::IndexEvent::Upsert {
            kb: kb_slug("notes"),
            object_key: object_path("queued.md"),
            etag: "\"queued\"".to_string(),
            mtime: 0,
        })
        .expect("queue accepts the prefilled event");

    let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    let body = response_body(resp).await;
    assert!(body.contains("backend_unavailable"));
    assert!(body.contains("destination write succeeded but destination index event failed"));
    assert!(storage.get_stored("notes", "dst.md").await.is_some());
    assert!(storage.get_stored("notes", "src.md").await.is_some());
}

#[tokio::test]
async fn test_move_returns_503_when_destination_upsert_backpressured() {
    let storage = empty_storage();
    storage
        .insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"")
        .await;
    let (indexer_tx, _rx) = mpsc::channel(1);
    indexer_tx
        .try_send(notedthat_indexer::IndexEvent::Upsert {
            kb: kb_slug("notes"),
            object_key: object_path("queued.md"),
            etag: "\"queued\"".to_string(),
            mtime: 0,
        })
        .expect("queue accepts the prefilled event");

    let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    assert!(response_body(resp).await.contains(
        "destination write succeeded but destination index event failed; source unchanged. Retry MOVE to re-enqueue destination index event."
    ));
    assert!(storage.get_stored("notes", "dst.md").await.is_some());
    assert!(storage.get_stored("notes", "src.md").await.is_some());
}

#[tokio::test]
async fn test_move_returns_503_when_source_tombstone_backpressured_after_destination_put() {
    let storage = empty_storage();
    storage
        .insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"")
        .await;
    let (indexer_tx, _rx) = mpsc::channel(2);
    indexer_tx
        .try_send(notedthat_indexer::IndexEvent::Upsert {
            kb: kb_slug("notes"),
            object_key: object_path("queued.md"),
            etag: "\"queued\"".to_string(),
            mtime: 0,
        })
        .expect("queue accepts the prefilled event");

    let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
    assert!(response_body(resp).await.contains(
        "destination write succeeded and source deleted from storage, but source search-index tombstone failed — search may return stale entries for the source path until retry or reindex. Send DELETE for the source to re-enqueue its tombstone — DELETE of a missing key is idempotent and still enqueues, whereas a retried MOVE would find no source."
    ));
    assert!(storage.get_stored("notes", "dst.md").await.is_some());
    assert!(storage.get_stored("notes", "src.md").await.is_none());
}

#[tokio::test]
async fn test_source_not_found_returns_404() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/missing.md")
                .header("destination", "/webdav/notes/dest.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_put_to_root_returns_400() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_delete_to_kb_root_returns_400() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("DELETE")
                .uri("/webdav/notes/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_move_collection_source_returns_403_no_collection_move() {
    let storage = empty_storage();
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/")
                .header("destination", "/webdav/notes/dest.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(response_body(resp).await.contains("no-collection-move"));
}

#[tokio::test]
async fn encoded_uri_put_stores_decoded_key() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/Untitled%201.canvas")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    // The key should be stored DECODED as "Untitled 1.canvas", not encoded as "Untitled%201.canvas"
    assert!(
        storage
            .get_stored("notes", "Untitled 1.canvas")
            .await
            .is_some(),
        "expected decoded key 'Untitled 1.canvas' to be stored"
    );
    assert!(
        storage
            .get_stored("notes", "Untitled%201.canvas")
            .await
            .is_none(),
        "encoded key 'Untitled%201.canvas' must not be stored"
    );
}

#[tokio::test]
async fn encoded_uri_put_multi_segment() {
    // Multi-segment path with percent-encoded directory name proves split-before-decode:
    // raw '/' separates segments, then %20 in "my%20folder" decodes within that segment.
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/my%20folder/notes.md")
                .header("content-type", "text/markdown")
                .body(Body::from("# Notes"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        storage
            .get_stored("notes", "my folder/notes.md")
            .await
            .is_some(),
        "expected decoded multi-segment key 'my folder/notes.md'"
    );
}

#[tokio::test]
async fn encoded_uri_put_literal_percent_round_trips() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/file%25.md")
                .header("content-type", "text/markdown")
                .body(Body::from("# Percent"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(resp.status().is_success());
    assert!(
        storage.get_stored("notes", "file%.md").await.is_some(),
        "expected decoded literal percent key 'file%.md'"
    );
    assert!(
        storage.get_stored("notes", "file%25.md").await.is_none(),
        "encoded key 'file%25.md' must not be stored"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn edge_case_uri_segment_decoding_matrix() {
    struct Case {
        name: &'static str,
        raw_uri: &'static str,
        should_succeed: bool,
        stored_key: Option<&'static str>,
    }

    let cases = [
        Case {
            name: "reject_empty_middle_segment",
            raw_uri: "/webdav/notes//file.md",
            should_succeed: false,
            stored_key: None,
        },
        Case {
            name: "reject_double_leading_slash",
            raw_uri: "/webdav//notes/file.md",
            should_succeed: false,
            stored_key: None,
        },
        Case {
            name: "reject_decoded_slash_in_segment",
            raw_uri: "/webdav/notes/%2Ffile.md",
            should_succeed: false,
            stored_key: None,
        },
        Case {
            name: "reject_decoded_parent_segment",
            raw_uri: "/webdav/notes/%2E%2E/foo.md",
            should_succeed: false,
            stored_key: None,
        },
        Case {
            name: "allow_leading_dot_filename",
            raw_uri: "/webdav/notes/%2Efoo.md",
            should_succeed: true,
            stored_key: Some(".foo.md"),
        },
        Case {
            name: "reject_decoded_backslash",
            raw_uri: "/webdav/notes/file%5Cbad.md",
            should_succeed: false,
            stored_key: None,
        },
        Case {
            name: "reject_decoded_nul",
            raw_uri: "/webdav/notes/file%00.md",
            should_succeed: false,
            stored_key: None,
        },
        Case {
            name: "allow_literal_percent_filename",
            raw_uri: "/webdav/notes/file%25.md",
            should_succeed: true,
            stored_key: Some("file%.md"),
        },
        Case {
            name: "allow_query_not_part_of_path",
            raw_uri: "/webdav/notes/file%3Fname.md?ignored=1",
            should_succeed: true,
            stored_key: Some("file?name.md"),
        },
        Case {
            name: "reject_decoded_slash_in_middle_segment",
            raw_uri: "/webdav/notes/segment%2Fwith-slash/file.md",
            should_succeed: false,
            stored_key: None,
        },
    ];

    for case in cases {
        let storage = empty_storage();
        let resp = app(storage.clone())
            .oneshot(
                HttpRequest::builder()
                    .method("PUT")
                    .uri(case.raw_uri)
                    .header("content-type", "text/markdown")
                    .body(Body::from(case.name))
                    .unwrap(),
            )
            .await
            .unwrap();

        if case.should_succeed {
            assert!(
                resp.status().is_success(),
                "{} should succeed, got {}",
                case.name,
                resp.status()
            );
            let stored_key = case.stored_key.expect("success case stores a key");
            assert!(
                storage.get_stored("notes", stored_key).await.is_some(),
                "{} should store decoded key {stored_key:?}",
                case.name
            );
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{} should reject malformed segment",
                case.name
            );
            assert!(
                storage.ops().is_empty(),
                "{} must not hit storage",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn encoded_uri_put_unicode() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/%E6%97%A5%E6%9C%AC%E8%AA%9E.md")
                .header("content-type", "text/markdown")
                .body(Body::from("# Japanese"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        storage.get_stored("notes", "日本語.md").await.is_some(),
        "expected decoded unicode key '日本語.md'"
    );
}

#[tokio::test]
async fn encoded_uri_put_reserved_chars() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/file%23with%3Fchars.md")
                .header("content-type", "text/markdown")
                .body(Body::from("# Reserved"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        storage
            .get_stored("notes", "file#with?chars.md")
            .await
            .is_some(),
        "expected decoded key 'file#with?chars.md'"
    );
}

#[tokio::test]
async fn encoded_uri_put_non_utf8_returns_400() {
    let storage = empty_storage();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/%FF%FE.md")
                .header("content-type", "text/markdown")
                .body(Body::from("# Bad"))
                .unwrap(),
        )
        .await
        .unwrap();
    // Non-UTF-8 percent sequences must be rejected with 400
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Consistent with existing write-method 400s: no x-request-id header
    assert!(
        resp.headers().get("x-request-id").is_none(),
        "write-method 400 must not include x-request-id"
    );
    // Nothing was stored
    assert!(storage.ops().is_empty());
}

#[tokio::test]
async fn encoded_destination_move_decodes_key() {
    let storage = empty_storage();
    storage
        .insert(
            "notes",
            "source.md",
            Bytes::from_static(b"source content"),
            "\"etag-source\"",
        )
        .await;
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/source.md")
                .header("destination", "/webdav/notes/renamed%20file.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Destination key must be decoded
    assert!(
        storage
            .get_stored("notes", "renamed file.md")
            .await
            .is_some(),
        "expected decoded destination key 'renamed file.md'"
    );
    // Source must be gone (MOVE deletes source)
    assert!(
        storage.get_stored("notes", "source.md").await.is_none(),
        "MOVE source should be deleted"
    );
}

#[tokio::test]
async fn encoded_destination_copy_decodes_key() {
    let storage = empty_storage();
    storage
        .insert(
            "notes",
            "source.md",
            Bytes::from_static(b"source content"),
            "\"etag-source\"",
        )
        .await;
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/source.md")
                .header("destination", "/webdav/notes/renamed%20file.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Destination key must be decoded
    assert!(
        storage
            .get_stored("notes", "renamed file.md")
            .await
            .is_some(),
        "expected decoded destination key 'renamed file.md'"
    );
    // Source must still exist (COPY keeps source)
    assert!(
        storage.get_stored("notes", "source.md").await.is_some(),
        "COPY source should still exist"
    );
}

#[tokio::test]
async fn destination_with_fragment_returns_400_before_uri_parse() {
    let storage = empty_storage();
    storage
        .insert(
            "notes",
            "source.md",
            Bytes::from_static(b"source content"),
            "\"etag-source\"",
        )
        .await;
    let resp = app(storage)
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/source.md")
                .header("host", "localhost")
                .header("destination", "http://localhost/notes/file.md#fragment")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
