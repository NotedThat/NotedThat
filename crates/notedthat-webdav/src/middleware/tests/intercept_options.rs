use super::super::*;
use axum::{Router, body::Body, http::Request as HttpRequest, middleware::from_fn, routing::any};
use tower::util::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/webdav", any(|| async { "inner handler reached" }))
        .layer(from_fn(intercept_options))
}

#[tokio::test]
async fn test_options_returns_204_dav_1() {
    let req = HttpRequest::builder()
        .method("OPTIONS")
        .uri("/webdav")
        .body(Body::empty())
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let dav = resp.headers().get("dav").unwrap();
    assert_eq!(dav.to_str().unwrap(), "1");
    assert!(!dav.to_str().unwrap().contains('2'));
    assert!(!dav.to_str().unwrap().contains('3'));
    assert!(resp.headers().contains_key("allow"));
}

#[tokio::test]
async fn test_options_body_empty() {
    let req = HttpRequest::builder()
        .method("OPTIONS")
        .uri("/webdav")
        .body(Body::empty())
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_non_options_passes_through() {
    let req = HttpRequest::builder()
        .method("GET")
        .uri("/webdav")
        .body(Body::empty())
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}
