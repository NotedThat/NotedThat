//! Integration tests for the `WebDAV` middleware stack.
//!
//! Tests exercise the full middleware chain using axum's `oneshot` pattern.
//! No testcontainers — all storage calls use in-memory mocks.

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use base64::Engine as _;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath,
    ObjectRead, PutOutcome, Storage, StorageError,
};
use notedthat_webdav::{router::build_router, state::WebDavState};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// MockStorage
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockStorage;

#[async_trait]
impl Storage for MockStorage {
    async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        unimplemented!()
    }

    async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
        unimplemented!()
    }

    async fn write_manifest(
        &self,
        _kb: &KbSlug,
        _manifest: &KbManifest,
    ) -> Result<(), StorageError> {
        unimplemented!()
    }

    async fn head_object(
        &self,
        _kb: &KbSlug,
        path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        Err(StorageError::NotFound {
            key: path.as_str().to_string(),
        })
    }

    async fn get_object(
        &self,
        _kb: &KbSlug,
        path: &ObjectPath,
        _range: Option<Vec<ByteRange>>,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        Ok(ObjectRead {
            bytes: Bytes::from_static(b"test content"),
            meta: ObjectMeta {
                key: path.as_str().to_string(),
                size: 12,
                last_modified: Some(0),
                content_type: Some("text/markdown".to_string()),
                etag: Some("\"src-etag\"".to_string()),
            },
            content_range: None,
        })
    }

    async fn put_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _bytes: Bytes,
        _content_type: Option<&str>,
        _conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        Ok(PutOutcome {
            etag: Some("\"test-etag\"".to_string()),
        })
    }

    async fn delete_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn list_objects(
        &self,
        _kb: &KbSlug,
        _prefix: Option<&str>,
        _limit: u32,
        _cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        Ok(ListResponse {
            objects: vec![],
            truncated: false,
            next_cursor: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_state() -> WebDavState {
    let (tx, _rx) = mpsc::channel(100);
    WebDavState {
        username: Arc::new("testuser".to_string()),
        password: Arc::new("testpass".to_string()),
        storage: Arc::new(MockStorage),
        declared_kbs: Arc::new({
            let mut m = BTreeMap::new();
            m.insert(
                "notes".to_string(),
                KbSlug::try_new("notes").expect("valid slug"),
            );
            m.insert(
                "scratch".to_string(),
                KbSlug::try_new("scratch").expect("valid slug"),
            );
            m
        }),
        indexer_tx: tx,
    }
}

fn basic_auth(user: &str, pass: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {encoded}")
}

fn good_auth() -> String {
    basic_auth("testuser", "testpass")
}

const PROPFIND_BODY: &str =
    r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#;

async fn body_string(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// Basic-auth tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_auth_returns_401() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/")
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_username_returns_401() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/")
        .header("Authorization", basic_auth("baduser", "testpass"))
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_password_returns_401() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/")
        .header("Authorization", basic_auth("testuser", "badpass"))
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn correct_credentials_reach_handler() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/")
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
}

#[tokio::test]
async fn www_authenticate_header_present_on_401() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers().get("www-authenticate").unwrap(),
        "Basic realm=\"NotedThat\""
    );
}

// ---------------------------------------------------------------------------
// OPTIONS interception
// ---------------------------------------------------------------------------

#[tokio::test]
async fn options_returns_204_dav_1() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let dav = resp
        .headers()
        .get("dav")
        .expect("DAV header must be present");
    assert_eq!(dav.to_str().unwrap(), "1");
}

#[tokio::test]
async fn options_dav_header_not_class_2_or_3() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/notes/test.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let dav_value = resp
        .headers()
        .get("dav")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert!(!dav_value.contains('2'), "DAV header must not contain '2'");
    assert!(!dav_value.contains('3'), "DAV header must not contain '3'");
}

// ---------------------------------------------------------------------------
// PROPPATCH interception
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proppatch_returns_405() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPPATCH").unwrap())
        .uri("/notes/test.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------------------
// LOCK / UNLOCK interception
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lock_returns_405() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"LOCK").unwrap())
        .uri("/notes/test.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn unlock_returns_405() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"UNLOCK").unwrap())
        .uri("/notes/test.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------------------
// Write-method interception (PUT / DELETE / MOVE)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_calls_commit_and_returns_201() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/notes/test.md")
        .header("Authorization", good_auth())
        .header("Content-Type", "text/markdown")
        .body(Body::from("# Test Note"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(resp.headers().contains_key("etag"));
}

#[tokio::test]
async fn delete_returns_204() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/notes/test.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn move_with_cross_kb_destination_returns_403() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"MOVE").unwrap())
        .uri("/notes/source.md")
        .header("Authorization", good_auth())
        .header("Host", "localhost:8081")
        .header("Destination", "http://localhost:8081/scratch/dest.md")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_string(resp).await;
    assert!(
        body.contains("cannot-modify-source"),
        "expected <nt:cannot-modify-source/> in body, got: {body}"
    );
}

#[tokio::test]
async fn move_with_missing_destination_returns_400() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"MOVE").unwrap())
        .uri("/notes/source.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn move_cross_server_returns_502() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"MOVE").unwrap())
        .uri("/notes/source.md")
        .header("Authorization", good_auth())
        .header("Host", "localhost:8081")
        .header("Destination", "http://other-host.example.com/notes/dest.md")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = body_string(resp).await;
    assert!(
        body.contains("destination-different-server"),
        "expected <nt:destination-different-server/> in body, got: {body}"
    );
}

#[tokio::test]
async fn options_without_auth_returns_401() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "OPTIONS without auth must return 401"
    );
}

// ---------------------------------------------------------------------------
// Read-method path normalization (RED gate)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_methods_reject_encoded_dotdot_get() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method("GET")
        .uri("/notes/%2e%2e/scratch/secret.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn read_methods_reject_encoded_dotdot_propfind() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/notes/%2e%2e/scratch/")
        .header("Authorization", good_auth())
        .header("Depth", "1")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn propfind_traversal_rejected_before_size_cap() {
    // This test verifies that intercept_read_methods (path validation)
    // runs BEFORE intercept_propfind_too_large (size cap).
    // A bad-path PROPFIND must return 400 (from intercept_read_methods),
    // NOT 207 or 507 (which would indicate wrong layer ordering).
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/notes/%2e%2e/scratch/")
        .header("Authorization", good_auth())
        .header("Depth", "1")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "bad-path PROPFIND must return 400 from intercept_read_methods, not from dav-server or size-cap layer"
    );
}

#[tokio::test]
async fn read_matrix_get_dotdot_declared() {
    let app = build_router(make_state());
    let uri = "/notes/../secret.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_single_dot_declared() {
    let app = build_router(make_state());
    let uri = "/notes/./hello.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_empty_segment_declared() {
    let app = build_router(make_state());
    let uri = "/notes//hello.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_encoded_slash_declared() {
    let app = build_router(make_state());
    let uri = "/notes/foo%2fbar.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_encoded_backslash_declared() {
    let app = build_router(make_state());
    let uri = "/notes/foo%5cbar.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_head_dotdot_declared() {
    let app = build_router(make_state());
    let uri = "/notes/%2e%2e/hello.md";
    let req = Request::builder()
        .method("HEAD")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_dotdot_declared() {
    let app = build_router(make_state());
    let uri = "/notes/%2e%2e/scratch/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_encoded_slash_declared() {
    let app = build_router(make_state());
    let uri = "/notes/foo%2fbar/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_dotdot_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/%2e%2e/notes/hello.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_head_dotdot_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/%2e%2e/notes/hello.md";
    let req = Request::builder()
        .method("HEAD")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_dotdot_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/%2e%2e/notes/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_dotslash_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/./x.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_empty_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown//x.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_encoded_slash_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/foo%2fbar/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_get_encoded_dotdot_mid_segment() {
    let app = build_router(make_state());
    let uri = "/notes/%2e%2e/scratch/deep.md";
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_double_slash_root() {
    let app = build_router(make_state());
    let uri = "//notes/hello";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_single_dot_declared() {
    let app = build_router(make_state());
    let uri = "/notes/./folder/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_empty_segment_declared() {
    let app = build_router(make_state());
    let uri = "/notes//folder/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_encoded_backslash_declared() {
    let app = build_router(make_state());
    let uri = "/notes/foo%5cbar/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_encoded_backslash_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/foo%5cbar/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_single_dot_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown/./x/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn read_matrix_propfind_empty_segment_non_declared() {
    let app = build_router(make_state());
    let uri = "/unknown//x/";
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri(uri)
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
}

#[tokio::test]
async fn get_legitimate_object_not_rejected() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method("GET")
        .uri("/notes/hello.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "legitimate GET must not be rejected by intercept_read_methods"
    );
}

#[tokio::test]
async fn propfind_root_not_rejected() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/")
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::MULTI_STATUS,
        "PROPFIND / must return 207 listing declared KBs"
    );
}

#[tokio::test]
async fn propfind_kb_root_not_rejected() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/notes/")
        .header("Authorization", good_auth())
        .header("Depth", "1")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "PROPFIND on KB root must not be rejected by intercept_read_methods"
    );
}

#[tokio::test]
async fn propfind_collection_prefix_trailing_slash_returns_207() {
    // LOAD-BEARING: validates that validate_read_uri_path tolerates ONE trailing /
    // A PROPFIND on a folder-like collection path (e.g. /notes/folder/) must
    // NOT be rejected by intercept_read_methods with 400.
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/notes/folder/")
        .header("Authorization", good_auth())
        .header("Depth", "1")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "PROPFIND on collection path with trailing slash must NOT be rejected by intercept_read_methods"
    );
}

#[tokio::test]
async fn head_collection_prefix_trailing_slash_not_rejected() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/notes/folder/")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "HEAD on collection path with trailing slash must not be rejected by intercept_read_methods"
    );
}
