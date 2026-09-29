use super::super::*;
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request as HttpRequest,
    middleware::from_fn_with_state,
    routing::get,
};
use base64::Engine as _;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath,
    ObjectRead, PutOutcome, Storage, StorageError,
};
use std::{collections::BTreeMap, sync::Arc};
use tower::util::ServiceExt;
use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};

#[derive(Default)]
struct MockStorage;

fn unavailable() -> StorageError {
    StorageError::BackendUnavailable {
        message: "mock storage is not used by auth middleware".to_string(),
    }
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
        _kb: &KbSlug,
        _path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        Err(unavailable())
    }

    async fn get_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _range: Option<ByteRange>,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        Err(unavailable())
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
        _kb: &KbSlug,
        _path: &ObjectPath,
        _bytes: Bytes,
        _content_type: Option<&str>,
        _conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        Err(unavailable())
    }

    async fn put_staged_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _body: notedthat_core::StagedBody,
        _content_type: Option<&str>,
        _conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        Err(unavailable())
    }

    async fn copy_object(
        &self,
        _kb: &KbSlug,
        _source: &ObjectPath,
        _destination: &ObjectPath,
        _options: notedthat_core::CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        Err(unavailable())
    }

    async fn delete_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        Err(unavailable())
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

fn test_state() -> WebDavState {
    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
    WebDavState {
        authenticator: Arc::new(
            notedthat_core::Authenticator::new("test-service-token")
                .with_basic("testuser".to_string(), "testpass".to_string()),
        ),
        storage: Arc::new(MockStorage),
        staging_config: notedthat_core::StagingConfig::default(),
        declared_kbs: Arc::new(BTreeMap::new()),
        access_policies: Arc::new(BTreeMap::new()),
        indexer_tx: (&indexer_tx).into(),
        events: None,
        index_health: Arc::new(notedthat_indexer::IndexHealth::new()),
    }
}

fn app() -> Router {
    let state = test_state();
    Router::new()
        .route("/webdav", get(|| async { "ok" }))
        .layer(from_fn_with_state(state, basic_auth_middleware))
}

fn app_with_request_id() -> Router {
    let state = test_state();
    Router::new()
        .route("/webdav", get(|| async { "ok" }))
        .layer(from_fn_with_state(state.clone(), basic_auth_middleware))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

fn basic_header(username: &str, password: &str) -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
}

#[tokio::test]
async fn test_rejects_missing_auth() {
    let resp = app()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers().get("www-authenticate").unwrap(),
        "Basic realm=\"NotedThat\""
    );
}

#[tokio::test]
async fn test_rejects_malformed_auth() {
    let resp = app()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .header("authorization", "Basic garbage!")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_rejects_wrong_username() {
    let resp = app()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .header("authorization", basic_header("wronguser", "testpass"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_rejects_wrong_password() {
    let resp = app()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .header("authorization", basic_header("testuser", "wrongpass"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_accepts_correct_credentials() {
    let resp = app()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .header("authorization", basic_header("testuser", "testpass"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_401_body_does_not_leak_credentials() {
    let resp = app()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .header("authorization", basic_header("testuser", "wrongpass"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body.contains("testuser"));
    assert!(!body.contains("testpass"));
}

#[tokio::test]
async fn test_401_contains_request_id_header() {
    let resp = app_with_request_id()
        .oneshot(
            HttpRequest::builder()
                .uri("/webdav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().contains_key("x-request-id"));
}
