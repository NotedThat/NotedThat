use super::super::*;
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request as HttpRequest,
    middleware::from_fn_with_state,
    routing::any,
};
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, PutOutcome, StagedBody, Storage, StorageError,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    sync::Mutex,
};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tower::util::ServiceExt;

#[derive(Clone)]
struct StoredObject {
    bytes: Bytes,
    content_type: Option<String>,
    etag: String,
}

#[derive(Default)]
struct MockStorage {
    objects: Mutex<HashMap<String, StoredObject>>,
    calls: Mutex<Vec<&'static str>>,
    copy_options: Mutex<Vec<CopyObjectOptions>>,
    staged_paths: Mutex<Vec<std::path::PathBuf>>,
    staged_lengths: Mutex<Vec<u64>>,
    next_etag: Mutex<u64>,
    race_destination_before_copy: Mutex<bool>,
    change_source_before_copy: Mutex<bool>,
    change_source_after_copy: Mutex<bool>,
}

impl MockStorage {
    fn key(kb: &KbSlug, path: &ObjectPath) -> String {
        format!("{}/{}", kb.as_str(), path.as_str())
    }

    fn record(&self, call: &'static str) {
        self.calls.lock().expect("mutex not poisoned").push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("mutex not poisoned").clone()
    }

    fn insert(&self, kb: &str, path: &str, bytes: impl Into<Bytes>, etag: &str) {
        self.insert_with_content_type(kb, path, bytes, etag, "text/markdown");
    }

    fn insert_with_content_type(
        &self,
        kb: &str,
        path: &str,
        bytes: impl Into<Bytes>,
        etag: &str,
        content_type: &str,
    ) {
        self.objects.lock().expect("mutex not poisoned").insert(
            format!("{kb}/{path}"),
            StoredObject {
                bytes: bytes.into(),
                content_type: Some(content_type.to_string()),
                etag: etag.to_string(),
            },
        );
    }

    fn get_stored(&self, kb: &str, path: &str) -> Option<StoredObject> {
        self.objects
            .lock()
            .expect("mutex not poisoned")
            .get(&format!("{kb}/{path}"))
            .cloned()
    }

    fn copy_options(&self) -> Vec<CopyObjectOptions> {
        self.copy_options
            .lock()
            .expect("mutex not poisoned")
            .clone()
    }

    fn staged_paths(&self) -> Vec<std::path::PathBuf> {
        self.staged_paths
            .lock()
            .expect("mutex not poisoned")
            .clone()
    }

    fn staged_lengths(&self) -> Vec<u64> {
        self.staged_lengths
            .lock()
            .expect("mutex not poisoned")
            .clone()
    }

    fn race_destination_before_copy(&self) {
        *self
            .race_destination_before_copy
            .lock()
            .expect("mutex not poisoned") = true;
    }

    fn change_source_after_copy(&self) {
        *self
            .change_source_after_copy
            .lock()
            .expect("mutex not poisoned") = true;
    }

    fn change_source_before_copy(&self) {
        *self
            .change_source_before_copy
            .lock()
            .expect("mutex not poisoned") = true;
    }
}

fn unavailable() -> StorageError {
    StorageError::BackendUnavailable {
        message: "mock storage method is not configured for this test".to_string(),
    }
}

fn object_meta(key: String, object: &StoredObject) -> ObjectMeta {
    ObjectMeta {
        key,
        size: object.bytes.len() as u64,
        last_modified: Some(1),
        content_type: object.content_type.clone(),
        etag: Some(object.etag.clone()),
    }
}

fn check_if_match(
    conditionals: &ConditionalHeaders,
    object: Option<&StoredObject>,
) -> Result<(), StorageError> {
    if let Some(if_match) = conditionals.if_match.as_deref()
        && object.is_none_or(|stored| stored.etag != if_match)
    {
        return Err(StorageError::PreconditionFailed);
    }
    Ok(())
}

#[async_trait]
impl Storage for MockStorage {
    async fn probe(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        Ok(())
    }

    async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        Err(unavailable())
    }

    async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
        Err(unavailable())
    }

    async fn write_manifest(
        &self,
        _kb: &KbSlug,
        _manifest: &KbManifest,
    ) -> Result<(), StorageError> {
        Err(unavailable())
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        self.record("head_object");
        let key = Self::key(kb, path);
        let objects = self.objects.lock().expect("mutex not poisoned");
        let object = objects.get(&key);
        check_if_match(&conditionals, object)?;
        object
            .map(|stored| object_meta(path.as_str().to_string(), stored))
            .ok_or(StorageError::NotFound { key })
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        self.record("get_object");
        let key = Self::key(kb, path);
        let objects = self.objects.lock().expect("mutex not poisoned");
        let object = objects
            .get(&key)
            .ok_or_else(|| StorageError::NotFound { key: key.clone() })?;
        check_if_match(&conditionals, Some(object))?;
        Ok(ObjectRead {
            bytes: object.bytes.clone(),
            meta: object_meta(path.as_str().to_string(), object),
            content_range: None,
        })
    }

    async fn get_object_stream(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _range: Option<ByteRange>,
        _conditionals: ConditionalHeaders,
    ) -> Result<notedthat_core::ObjectStream, StorageError> {
        Err(unavailable())
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        self.record("put_object");
        let key = Self::key(kb, path);
        let mut objects = self.objects.lock().expect("mutex not poisoned");
        check_if_match(&conditionals, objects.get(&key))?;

        let mut next_etag = self.next_etag.lock().expect("mutex not poisoned");
        *next_etag += 1;
        let etag = format!("\"etag-{next_etag}\"");
        let replaced = objects.insert(
            key,
            StoredObject {
                bytes,
                content_type: content_type.map(str::to_string),
                etag: etag.clone(),
            },
        );
        Ok(PutOutcome {
            etag: Some(etag),
            created: replaced.is_none(),
        })
    }

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let staged_len = body.len();
        if let Some(path) = body.file_path() {
            self.staged_paths
                .lock()
                .expect("mutex not poisoned")
                .push(path.to_path_buf());
        }
        let mut reader = body.open().await.map_err(|source| StorageError::Other {
            source: Box::new(source),
        })?;
        let bytes = if body.is_file() {
            let mut buffer = vec![0_u8; 64 * 1024];
            let mut read = 0_u64;
            loop {
                let count =
                    reader
                        .read(&mut buffer)
                        .await
                        .map_err(|source| StorageError::Other {
                            source: Box::new(source),
                        })?;
                if count == 0 {
                    break;
                }
                read += u64::try_from(count).unwrap();
            }
            assert_eq!(read, staged_len);
            Vec::new()
        } else {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .await
                .map_err(|source| StorageError::Other {
                    source: Box::new(source),
                })?;
            bytes
        };
        self.staged_lengths
            .lock()
            .expect("mutex not poisoned")
            .push(staged_len);
        self.put_object(kb, path, Bytes::from(bytes), content_type, conditionals)
            .await
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        self.record("copy_object");
        self.copy_options
            .lock()
            .expect("mutex not poisoned")
            .push(options.clone());
        let source_key = Self::key(kb, source);
        let destination_key = Self::key(kb, destination);
        let mut objects = self.objects.lock().expect("mutex not poisoned");
        if *self
            .change_source_before_copy
            .lock()
            .expect("mutex not poisoned")
            && let Some(source) = objects.get_mut(&source_key)
        {
            source.etag = "\"changed\"".to_string();
        }
        let source_object =
            objects
                .get(&source_key)
                .cloned()
                .ok_or_else(|| StorageError::NotFound {
                    key: source_key.clone(),
                })?;
        if options
            .source_if_match
            .as_deref()
            .is_some_and(|etag| etag != source_object.etag)
        {
            return Err(StorageError::PreconditionFailed);
        }
        if *self
            .race_destination_before_copy
            .lock()
            .expect("mutex not poisoned")
        {
            objects.insert(
                destination_key.clone(),
                StoredObject {
                    bytes: Bytes::from_static(b"racing writer"),
                    content_type: Some("text/plain".to_string()),
                    etag: "\"race\"".to_string(),
                },
            );
        }
        if options.destination_if_none_match.as_deref() == Some("*")
            && objects.contains_key(&destination_key)
        {
            return Err(StorageError::PreconditionFailed);
        }
        let mut copied = source_object;
        copied.content_type = options.content_type;
        let etag = copied.etag.clone();
        let created = objects.insert(destination_key, copied).is_none();
        if *self
            .change_source_after_copy
            .lock()
            .expect("mutex not poisoned")
            && let Some(source) = objects.get_mut(&source_key)
        {
            source.etag = "\"changed\"".to_string();
        }
        Ok(PutOutcome {
            etag: Some(etag),
            created,
        })
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        self.record("delete_object");
        let key = Self::key(kb, path);
        let mut objects = self.objects.lock().expect("mutex not poisoned");
        check_if_match(&conditionals, objects.get(&key))?;
        objects.remove(&key);
        Ok(())
    }

    async fn list_objects(
        &self,
        _kb: &KbSlug,
        _prefix: Option<&str>,
        _limit: u32,
        _cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        Err(unavailable())
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

fn test_state(storage: Arc<MockStorage>) -> WebDavState {
    let (indexer_tx, _rx) = mpsc::channel(1024);
    test_state_with_indexer_tx(storage, indexer_tx)
}

#[allow(clippy::needless_pass_by_value)]
fn test_state_with_indexer_tx(
    storage: Arc<MockStorage>,
    indexer_tx: mpsc::Sender<notedthat_indexer::IndexEvent>,
) -> WebDavState {
    let storage: Arc<dyn Storage> = storage;
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

fn app(storage: Arc<MockStorage>) -> Router {
    Router::new()
        .fallback(any(|| async { "inner handler reached" }))
        .layer(from_fn_with_state(
            test_state(storage),
            intercept_write_methods,
        ))
}

fn app_with_indexer_tx(
    storage: Arc<MockStorage>,
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
    storage: Arc<MockStorage>,
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
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    assert_eq!(resp.headers().get("etag").unwrap(), "\"etag-1\"");
    // No pre-write HEAD: the write itself reports that it created the object.
    assert_eq!(storage.calls(), vec!["put_object"]);
}

#[tokio::test]
async fn test_put_overwrite_returns_204() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "old.md", Bytes::from_static(b"old"), "\"old\"");
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
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "a.md", Bytes::from_static(b"body"), "\"a\"");
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
    assert_eq!(storage.calls(), vec!["head_object"]);
}

#[tokio::test]
async fn test_proppatch_on_a_knowledge_base_root_returns_207() {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage)
        .oneshot(proppatch("/webdav/notes/", &[], DISPLAYNAME))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
}

#[tokio::test]
async fn test_proppatch_honours_if_and_if_match() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "a.md", Bytes::from_static(b"body"), "\"a\"");
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
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "a.md", Bytes::from_static(b"body"), "\"a\"");
    let resp = app(storage)
        .oneshot(proppatch("/webdav/notes/a.md", &[], "<D:oops"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_put_with_if_match_wrong_etag_returns_412() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "old.md", Bytes::from_static(b"old"), "\"old\"");
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
    let storage = Arc::new(MockStorage::default());
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
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn test_put_md_with_octet_stream_stored_as_text_markdown() {
    let storage = Arc::new(MockStorage::default());
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
    let stored = storage.get_stored("notes", "sniff.md").unwrap();
    assert_eq!(stored.content_type.as_deref(), Some("text/markdown"));
}

#[tokio::test]
async fn test_put_to_non_declared_kb_returns_403() {
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    assert!(storage.get_stored("notes", "x.md").is_some());
}

#[tokio::test]
async fn test_delete_idempotent_returns_204() {
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "delete.md", Bytes::from_static(b"old"), "\"old\"");
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
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "y.md", Bytes::from_static(b"y"), "\"old\"");
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
    assert!(storage.get_stored("notes", "y.md").is_none());
}

#[tokio::test]
async fn test_move_missing_destination_returns_400() {
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
    storage.insert(
        "notes",
        "source.md",
        Bytes::from_static(b"source"),
        "\"source\"",
    );
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
        storage.calls(),
        vec!["head_object", "copy_object", "delete_object"]
    );
    assert!(storage.get_stored("notes", "source.md").is_none());
    assert!(storage.get_stored("notes", "dest.md").is_some());
}

#[tokio::test]
async fn test_copy_single_object_returns_201_and_calls_only_commit() {
    let storage = Arc::new(MockStorage::default());
    storage.insert(
        "notes",
        "source.md",
        Bytes::from_static(b"source"),
        "\"source\"",
    );
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
    assert_eq!(storage.calls(), vec!["head_object", "copy_object"]);
    assert!(storage.get_stored("notes", "source.md").is_some());
    assert!(storage.get_stored("notes", "copy.md").is_some());
    assert!(!storage.calls().contains(&"get_object"));
    assert_eq!(storage.copy_options()[0].destination_if_none_match, None);
}

#[tokio::test]
async fn test_copy_or_move_maps_destination_indexer_backpressure_to_503() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"");
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
    assert!(storage.get_stored("notes", "dst.md").is_some());
    assert!(storage.get_stored("notes", "src.md").is_some());
}

#[tokio::test]
async fn test_move_returns_503_when_destination_upsert_backpressured() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"");
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
    assert!(storage.get_stored("notes", "dst.md").is_some());
    assert!(storage.get_stored("notes", "src.md").is_some());
}

#[tokio::test]
async fn test_move_returns_503_when_source_tombstone_backpressured_after_destination_put() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"");
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
    assert!(storage.get_stored("notes", "dst.md").is_some());
    assert!(storage.get_stored("notes", "src.md").is_none());
}

#[tokio::test]
async fn test_source_not_found_returns_404() {
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
    let storage = Arc::new(MockStorage::default());
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
        storage.get_stored("notes", "Untitled 1.canvas").is_some(),
        "expected decoded key 'Untitled 1.canvas' to be stored"
    );
    assert!(
        storage.get_stored("notes", "Untitled%201.canvas").is_none(),
        "encoded key 'Untitled%201.canvas' must not be stored"
    );
}

#[tokio::test]
async fn encoded_uri_put_multi_segment() {
    // Multi-segment path with percent-encoded directory name proves split-before-decode:
    // raw '/' separates segments, then %20 in "my%20folder" decodes within that segment.
    let storage = Arc::new(MockStorage::default());
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
        storage.get_stored("notes", "my folder/notes.md").is_some(),
        "expected decoded multi-segment key 'my folder/notes.md'"
    );
}

#[tokio::test]
async fn encoded_uri_put_literal_percent_round_trips() {
    let storage = Arc::new(MockStorage::default());
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
        storage.get_stored("notes", "file%.md").is_some(),
        "expected decoded literal percent key 'file%.md'"
    );
    assert!(
        storage.get_stored("notes", "file%25.md").is_none(),
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
        let storage = Arc::new(MockStorage::default());
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
                storage.get_stored("notes", stored_key).is_some(),
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
                storage.calls().is_empty(),
                "{} must not hit storage",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn encoded_uri_put_unicode() {
    let storage = Arc::new(MockStorage::default());
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
        storage.get_stored("notes", "日本語.md").is_some(),
        "expected decoded unicode key '日本語.md'"
    );
}

#[tokio::test]
async fn encoded_uri_put_reserved_chars() {
    let storage = Arc::new(MockStorage::default());
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
        storage.get_stored("notes", "file#with?chars.md").is_some(),
        "expected decoded key 'file#with?chars.md'"
    );
}

#[tokio::test]
async fn encoded_uri_put_non_utf8_returns_400() {
    let storage = Arc::new(MockStorage::default());
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
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn encoded_destination_move_decodes_key() {
    let storage = Arc::new(MockStorage::default());
    storage.insert(
        "notes",
        "source.md",
        Bytes::from_static(b"source content"),
        "\"etag-source\"",
    );
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
        storage.get_stored("notes", "renamed file.md").is_some(),
        "expected decoded destination key 'renamed file.md'"
    );
    // Source must be gone (MOVE deletes source)
    assert!(
        storage.get_stored("notes", "source.md").is_none(),
        "MOVE source should be deleted"
    );
}

#[tokio::test]
async fn encoded_destination_copy_decodes_key() {
    let storage = Arc::new(MockStorage::default());
    storage.insert(
        "notes",
        "source.md",
        Bytes::from_static(b"source content"),
        "\"etag-source\"",
    );
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
        storage.get_stored("notes", "renamed file.md").is_some(),
        "expected decoded destination key 'renamed file.md'"
    );
    // Source must still exist (COPY keeps source)
    assert!(
        storage.get_stored("notes", "source.md").is_some(),
        "COPY source should still exist"
    );
}

#[tokio::test]
async fn destination_with_fragment_returns_400_before_uri_parse() {
    let storage = Arc::new(MockStorage::default());
    storage.insert(
        "notes",
        "source.md",
        Bytes::from_static(b"source content"),
        "\"etag-source\"",
    );
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
