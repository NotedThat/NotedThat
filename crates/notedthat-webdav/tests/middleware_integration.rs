//! Integration tests for the `WebDAV` middleware stack.
//!
//! Tests exercise the full middleware chain using axum's `oneshot` pattern.
//! No testcontainers — storage is an `InMemoryStorage`.

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use base64::Engine as _;
use notedthat_core::KbSlug;
use notedthat_core::testing::InMemoryStorage;
use notedthat_webdav::{router::build_router, state::WebDavState};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn declared_kbs() -> BTreeMap<String, KbSlug> {
    BTreeMap::from([
        (
            "notes".to_string(),
            KbSlug::try_new("notes").expect("valid slug"),
        ),
        (
            "scratch".to_string(),
            KbSlug::try_new("scratch").expect("valid slug"),
        ),
    ])
}

/// A state over an empty store with both declared knowledge bases provisioned.
fn make_state() -> WebDavState {
    make_state_with(InMemoryStorage::with_kbs(declared_kbs().values()))
}

/// A state whose store already holds `notes/<key>`, for a test that needs the object
/// to exist.
async fn state_with_note(key: &str) -> WebDavState {
    let storage = InMemoryStorage::with_kbs(declared_kbs().values());
    storage
        .seed("notes", key, "test content", Some("text/markdown"), None)
        .await;
    make_state_with(storage)
}

fn make_state_with(storage: InMemoryStorage) -> WebDavState {
    let (tx, _rx) = mpsc::channel(100);
    let declared = declared_kbs();
    WebDavState {
        authenticator: Arc::new(
            notedthat_core::Authenticator::new("test-service-token")
                .with_basic("testuser".to_string(), "testpass".to_string()),
        ),
        storage: Arc::new(storage),
        staging_config: notedthat_core::StagingConfig::default(),
        declared_kbs: Arc::new(declared.clone()),
        access_policies: Arc::new(notedthat_core::signed_in_policies(&declared)),
        indexer_tx: (&tx).into(),
        events: None,
        index_health: Arc::new(notedthat_indexer::IndexHealth::new()),
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
async fn router_serves_only_webdav_prefix() {
    // Given: the WebDAV router is mounted alongside other HTTP surfaces.
    let app = build_router(make_state());

    // When: callers request the old root and knowledge-base paths.
    let root = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let old_kb_path = app
        .oneshot(
            Request::builder()
                .uri("/notes/file.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Then: neither path is consumed by WebDAV or its authentication middleware.
    assert_eq!(root.status(), StatusCode::NOT_FOUND);
    assert_eq!(old_kb_path.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webdav_prefix_reaches_basic_authentication() {
    // Given: a request targets the scoped WebDAV root.
    let app = build_router(make_state());

    // When: the request has no credentials.
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::from_bytes(b"PROPFIND").unwrap())
                .uri("/webdav")
                .header("Depth", "0")
                .body(Body::from(PROPFIND_BODY))
                .unwrap(),
        )
        .await
        .unwrap();

    // Then: WebDAV's Basic challenge is returned from the scoped route.
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("www-authenticate").unwrap(),
        "Basic realm=\"NotedThat\""
    );
}

#[tokio::test]
async fn missing_auth_returns_401() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/webdav")
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
        .uri("/webdav")
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
        .uri("/webdav")
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
        .uri("/webdav")
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
        .uri("/webdav")
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
        .uri("/webdav")
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
        .uri("/webdav/notes/test.md")
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
async fn proppatch_of_a_missing_object_returns_404() {
    // PROPPATCH is a class-1 method (RFC 4918 §18.1), so it is answered, not 405.
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPPATCH").unwrap())
        .uri("/webdav/notes/test.md")
        .header("Authorization", good_auth())
        .body(Body::from(
            r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><D:displayname>x</D:displayname></D:prop></D:set></D:propertyupdate>"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// LOCK / UNLOCK interception
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lock_returns_405() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"LOCK").unwrap())
        .uri("/webdav/notes/test.md")
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
        .uri("/webdav/notes/test.md")
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
        .uri("/webdav/notes/test.md")
        .header("Authorization", good_auth())
        .header("Content-Type", "text/markdown")
        .body(Body::from("# Test Note"))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(resp.headers().contains_key("etag"));
}

/// `index`, `index/reconcile`, `events` and `search` at a knowledge base's
/// root are API routes, so an object under one of them could never be read
/// back over HTTP or MCP (#279). `WebDAV` refuses to create one, directly or as
/// a `MOVE` destination; nested keys are ordinary objects.
#[tokio::test]
async fn writes_to_reserved_root_keys_are_refused() {
    for key in ["index", "index/reconcile", "events", "search"] {
        let resp = build_router(make_state())
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/webdav/notes/{key}"))
                    .header("Authorization", good_auth())
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "PUT {key}");

        let resp = build_router(state_with_note("a.md").await)
            .oneshot(
                Request::builder()
                    .method(Method::from_bytes(b"MOVE").unwrap())
                    .uri("/webdav/notes/a.md")
                    .header("Authorization", good_auth())
                    .header("Destination", format!("/webdav/notes/{key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "MOVE to {key}");
    }

    let resp = build_router(make_state())
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/webdav/notes/drafts/index")
                .header("Authorization", good_auth())
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn delete_returns_204() {
    let app = build_router(state_with_note("test.md").await);
    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/webdav/notes/test.md")
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
        .uri("/webdav/notes/source.md")
        .header("Authorization", good_auth())
        .header("Host", "localhost:8081")
        .header(
            "Destination",
            "http://localhost:8081/webdav/scratch/dest.md",
        )
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
        .uri("/webdav/notes/source.md")
        .header("Authorization", good_auth())
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn move_with_destination_outside_webdav_returns_400() {
    // Given: a MOVE source is inside WebDAV but its destination names another surface.
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"MOVE").unwrap())
        .uri("/webdav/notes/source.md")
        .header("Authorization", good_auth())
        .header("Host", "localhost:8081")
        .header("Destination", "http://localhost:8081/api/v1/notes/dest.md")
        .body(Body::empty())
        .unwrap();

    // When: the scoped router handles the MOVE request.
    let resp = app.oneshot(req).await.unwrap();

    // Then: the destination is rejected before any storage operation.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn move_cross_server_returns_502() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"MOVE").unwrap())
        .uri("/webdav/notes/source.md")
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
        .uri("/webdav")
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
        .uri("/webdav/notes/%2e%2e/scratch/secret.md")
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
        .uri("/webdav/notes/%2e%2e/scratch/")
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
        .uri("/webdav/notes/%2e%2e/scratch/")
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
    let uri = "/webdav/notes/../secret.md";
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
    let uri = "/webdav/notes/./hello.md";
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
    let uri = "/webdav/notes//hello.md";
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
    let uri = "/webdav/notes/foo%2fbar.md";
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
    let uri = "/webdav/notes/foo%5cbar.md";
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
    let uri = "/webdav/notes/%2e%2e/hello.md";
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
    let uri = "/webdav/notes/%2e%2e/scratch/";
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
    let uri = "/webdav/notes/foo%2fbar/";
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
    let uri = "/webdav/unknown/%2e%2e/notes/hello.md";
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
    let uri = "/webdav/unknown/%2e%2e/notes/hello.md";
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
    let uri = "/webdav/unknown/%2e%2e/notes/";
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
    let uri = "/webdav/unknown/./x.md";
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
    let uri = "/webdav/unknown//x.md";
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
    let uri = "/webdav/unknown/foo%2fbar/";
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
    let uri = "/webdav/notes/%2e%2e/scratch/deep.md";
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
    let uri = "/webdav//notes/hello";
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
    let uri = "/webdav/notes/./folder/";
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
    let uri = "/webdav/notes//folder/";
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
    let uri = "/webdav/notes/foo%5cbar/";
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
    let uri = "/webdav/unknown/foo%5cbar/";
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
    let uri = "/webdav/unknown/./x/";
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
    let uri = "/webdav/unknown//x/";
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
    let app = build_router(state_with_note("hello.md").await);
    let req = Request::builder()
        .method("GET")
        .uri("/webdav/notes/hello.md")
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
    // Given: an authenticated PROPFIND targets the scoped WebDAV root.
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/webdav")
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(Body::from(PROPFIND_BODY))
        .unwrap();
    // When: dav-server produces the multistatus response.
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_string(resp).await;

    // Then: the canonical href retains the public WebDAV prefix.
    assert_eq!(
        status,
        StatusCode::MULTI_STATUS,
        "PROPFIND / must return 207 listing declared KBs"
    );
    assert!(
        body.contains("<D:href>/webdav/</D:href>"),
        "canonical root href must include /webdav; body: {body}"
    );
}

#[tokio::test]
async fn actual_http_propfind_keeps_webdav_in_canonical_href() {
    // Given: the scoped router is serving on a real ephemeral TCP listener.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, build_router(make_state()))
            .await
            .unwrap();
    });

    // When: a WebDAV client sends PROPFIND over HTTP.
    let response = reqwest::Client::new()
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://{address}/webdav"),
        )
        .header("Authorization", good_auth())
        .header("Depth", "0")
        .body(PROPFIND_BODY)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    server.abort();

    // Then: the wire response exposes the scoped canonical href.
    println!(
        "status={status} canonical_href_present={}",
        body.contains("<D:href>/webdav/</D:href>")
    );
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains("<D:href>/webdav/</D:href>"));
}

#[tokio::test]
async fn propfind_kb_root_not_rejected() {
    let app = build_router(make_state());
    let req = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").unwrap())
        .uri("/webdav/notes/")
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
        .uri("/webdav/notes/folder/")
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
        .uri("/webdav/notes/folder/")
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
