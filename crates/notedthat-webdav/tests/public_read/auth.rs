use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_webdav::router::build_router;
use tower::ServiceExt;

use super::fixture::{policy, request, response_body, state_with_policies};
use super::storage::MemoryStorage;

#[tokio::test]
async fn supplied_invalid_basic_never_falls_back_to_content_access() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([("private", "private.md")]));
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("private".to_string(), policy(&["content"]))]),
    ));

    for authorization in ["Basic !!!", "Basic dXNlcjpiYWQ=", "Bearer token"] {
        // When
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/webdav/private/private.md")
                    .header("Authorization", authorization)
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router response");

        // Then
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get("www-authenticate"),
            Some(&"Basic realm=\"NotedThat\"".parse().expect("valid header"))
        );
    }
}

#[tokio::test]
async fn invalid_basic_response_gives_safe_public_read_guidance_without_secrets() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([("private", "private.md")]));
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("private".to_string(), policy(&["content"]))]),
    ));

    // When
    let response = app
        .oneshot(
            Request::builder()
                .uri("/webdav/private/private.md")
                .header("Authorization", "Basic dXNlcjpiYWQ=")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("router response");

    // Then
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("www-authenticate"),
        Some(&"Basic realm=\"NotedThat\"".parse().expect("valid header"))
    );
    let body = response_body(response).await;
    assert!(
        body.contains("valid credentials are required"),
        "body: {body}"
    );
    assert!(
        body.contains("configured public-read capabilities"),
        "body: {body}"
    );
    assert!(
        body.contains("invalid credentials are not treated as anonymous"),
        "body: {body}"
    );
    assert!(!body.contains("user"), "body leaked username: {body}");
    assert!(!body.contains("pass"), "body leaked password: {body}");
    assert!(
        !body.contains("bad"),
        "body leaked supplied password: {body}"
    );
}

#[tokio::test]
async fn duplicate_authorization_headers_are_rejected_even_when_first_is_valid() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([("private", "private.md")]));
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("private".to_string(), policy(&["content"]))]),
    ));

    // When
    let response = app
        .oneshot(
            Request::builder()
                .uri("/webdav/private/private.md")
                .header("Authorization", "Basic dXNlcjpwYXNz")
                .header("Authorization", "Basic dXNlcjpiYWQ=")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("router response");

    // Then
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key("www-authenticate"));
}

#[tokio::test]
async fn malformed_path_is_rejected_but_malformed_basic_takes_precedence() {
    // Given
    let storage = Arc::new(MemoryStorage::default());
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("discoverable".to_string(), policy(&["content"]))]),
    ));

    // When
    let absent = app
        .clone()
        .oneshot(request("GET", "/discoverable/%2e%2e/private.md"))
        .await
        .expect("absent credentials response");
    let malformed = app
        .oneshot(
            Request::builder()
                .uri("/webdav/discoverable/%2e%2e/private.md")
                .header("Authorization", "Basic !!!")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("malformed credentials response");

    // Then
    assert_eq!(absent.status(), StatusCode::BAD_REQUEST);
    assert_eq!(malformed.status(), StatusCode::UNAUTHORIZED);
    assert!(malformed.headers().contains_key("www-authenticate"));
}
