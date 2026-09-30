use super::super::*;
use axum::{Router, body::Body, http::Request as HttpRequest, middleware::from_fn, routing::any};
use tower::util::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/webdav", any(|| async { "inner handler reached" }))
        .layer(from_fn(intercept_lock_unlock))
}

#[tokio::test]
async fn test_lock_returns_405() {
    let req = HttpRequest::builder()
        .method("LOCK")
        .uri("/webdav")
        .body(Body::empty())
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_unlock_returns_405() {
    let req = HttpRequest::builder()
        .method("UNLOCK")
        .uri("/webdav")
        .body(Body::empty())
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
