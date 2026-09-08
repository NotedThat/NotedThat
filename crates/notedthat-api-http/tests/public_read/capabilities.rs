use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_core::PublicReadCapability;
use tower::ServiceExt;

use super::fixture::{TOKEN, app, json, policy};

#[tokio::test]
async fn anonymous_discovery_is_filtered_while_authenticated_discovery_is_unfiltered() {
    // Given
    let app = app(BTreeMap::from([(
        "notes".to_string(),
        policy([PublicReadCapability::Discover]),
    )]))
    .await;

    // When
    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let authenticated = app
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(anonymous.status(), StatusCode::OK);
    let anonymous_json = json(anonymous).await;
    assert_eq!(
        anonymous_json["knowledgebases"],
        serde_json::json!(["notes"])
    );
    assert_eq!(authenticated.status(), StatusCode::OK);
    let authenticated_json = json(authenticated).await;
    assert_eq!(
        authenticated_json["knowledgebases"],
        serde_json::json!(["notes", "private"])
    );
}

#[tokio::test]
async fn anonymous_content_allows_get_and_head_but_rejects_internal_paths() {
    // Given
    let app = app(BTreeMap::from([(
        "notes".to_string(),
        policy([PublicReadCapability::Content]),
    )]))
    .await;

    // When
    let get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases/notes/public.md")
                .header("range", "bytes=0-5")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let head = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/v1/knowledgebases/notes/public.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let internal = app
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases/notes/.notedthat/manifest.json")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(get.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(internal.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json(internal).await["error"], "unauthorized");
}
