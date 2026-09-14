use notedthat_core::{Verb, Who};
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
        BTreeMap::from([("private".to_string(), policy(Who::Anyone, &[Verb::Read]))]),
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
        BTreeMap::from([("private".to_string(), policy(Who::Anyone, &[Verb::Read]))]),
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
    assert!(body.contains("access rules grant it"), "body: {body}");
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
        BTreeMap::from([("private".to_string(), policy(Who::Anyone, &[Verb::Read]))]),
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
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(Who::Anyone, &[Verb::Read]),
        )]),
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

async fn bearer_get(app: axum::Router, uri: &str, token: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .uri(format!("/webdav{uri}"))
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("valid request"),
    )
    .await
    .expect("router response")
}

#[tokio::test]
async fn a_bearer_service_token_is_accepted_on_webdav() {
    // Given — a private base only the credential holder may read.
    let storage = Arc::new(MemoryStorage::with_objects([("private", "private.md")]));
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("private".to_string(), policy(Who::SignedIn, &[Verb::Read]))]),
    ));

    // When / Then — a client that can set a header need not speak Basic.
    assert_eq!(
        bearer_get(app, "/private/private.md", super::fixture::SERVICE_TOKEN)
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_bearer_identity_token_is_bound_by_group_rules_on_webdav() {
    // Given — editors may read; everyone else signed in may only list.
    let storage = Arc::new(MemoryStorage::with_objects([("private", "private.md")]));
    let policy: notedthat_core::AccessPolicy = [
        notedthat_core::AccessRule::new(Who::SignedIn, [Verb::List]),
        notedthat_core::AccessRule::new(Who::Group("editors".into()), [Verb::Read]),
    ]
    .into_iter()
    .collect();
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("private".to_string(), policy)]),
    ));

    // When
    let alice = bearer_get(
        app.clone(),
        "/private/private.md",
        super::fixture::ALICE_TOKEN,
    )
    .await;
    let service = bearer_get(
        app.clone(),
        "/private/private.md",
        super::fixture::SERVICE_TOKEN,
    )
    .await;
    let unknown = bearer_get(app, "/private/private.md", "jwt-nobody").await;

    // Then — a verified but ungranted credential is 403, an unverifiable one
    // is the Basic challenge, because credentials might still change the answer.
    assert_eq!(alice.status(), StatusCode::OK);
    assert_eq!(service.status(), StatusCode::FORBIDDEN);
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
}
