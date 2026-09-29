use super::super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request as HttpRequest,
    middleware::from_fn_with_state,
    routing::get,
};
use base64::Engine as _;
use notedthat_core::testing::InMemoryStorage;
use std::{collections::BTreeMap, sync::Arc};
use tower::util::ServiceExt;
use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};

fn test_state() -> WebDavState {
    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
    WebDavState {
        authenticator: Arc::new(
            notedthat_core::Authenticator::new("test-service-token")
                .with_basic("testuser".to_string(), "testpass".to_string()),
        ),
        // Nothing is declared, so the middleware has no knowledge base to reach.
        storage: Arc::new(InMemoryStorage::default()),
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
