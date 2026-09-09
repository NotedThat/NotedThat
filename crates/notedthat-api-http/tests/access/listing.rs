use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_core::{AccessPolicy, Principal, Verb};
use tower::ServiceExt;

use super::fixture::{
    TOKEN, app, grant, grant_under, json, listed_keys, policy, signed_in_everything,
};

fn notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

async fn keys_for(app: axum::Router, uri: &str, token: Option<&str>) -> Vec<String> {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    listed_keys(response).await
}

#[tokio::test]
async fn a_listing_shows_every_key_the_credential_holder_may_see() {
    // Given
    let app = app(notes([signed_in_everything()])).await;

    // When
    let keys = keys_for(app, "/api/v1/knowledgebases/notes", Some(TOKEN)).await;

    // Then — including the internal namespace, which the credential holder
    // always reaches.
    assert!(keys.contains(&".notedthat/manifest.json".to_string()));
    assert!(keys.contains(&"internal/secret.md".to_string()));
}

#[tokio::test]
async fn a_prefix_scoped_list_grant_shows_only_keys_under_that_prefix() {
    // Given
    let app = app(notes([grant_under(
        Principal::Anyone,
        [Verb::List],
        &["public/**"],
    )]))
    .await;

    // When
    let keys = keys_for(app, "/api/v1/knowledgebases/notes", None).await;

    // Then
    assert_eq!(
        keys,
        vec![
            "public/deep/note.md".to_string(),
            "public/index.md".to_string()
        ],
        "only the granted subtree, and `public.md` is not under `public/`"
    );
}

#[tokio::test]
async fn a_listing_drops_the_internal_namespace_for_an_anonymous_caller() {
    // Given
    let app = app(notes([grant(Principal::Anyone, [Verb::List])])).await;

    // When
    let keys = keys_for(app, "/api/v1/knowledgebases/notes", None).await;

    // Then
    assert!(!keys.is_empty());
    assert!(
        !keys.iter().any(|key| key.starts_with(".notedthat")),
        "{keys:?}"
    );
}

#[tokio::test]
async fn a_caller_prefix_outside_the_granted_scope_lists_nothing() {
    // Given
    let app = app(notes([grant_under(
        Principal::Anyone,
        [Verb::List],
        &["public/**"],
    )]))
    .await;

    // When
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes?prefix=internal/")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then — an empty page, not an error: the caller asked a legitimate question
    // whose answer happens to be nothing.
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["objects"], serde_json::json!([]));
    assert_eq!(body["truncated"], serde_json::json!(false));
    assert_eq!(body["next_cursor"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_caller_prefix_inside_the_granted_scope_narrows_further() {
    // Given
    let app = app(notes([grant_under(
        Principal::Anyone,
        [Verb::List],
        &["public/**"],
    )]))
    .await;

    // When
    let keys = keys_for(
        app,
        "/api/v1/knowledgebases/notes?prefix=public/deep/",
        None,
    )
    .await;

    // Then
    assert_eq!(keys, vec!["public/deep/note.md".to_string()]);
}

#[tokio::test]
async fn listing_is_refused_without_a_list_grant() {
    // Given — `read` alone does not let a caller enumerate.
    let app = app(notes([grant(Principal::Anyone, [Verb::Read])])).await;

    // When
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn listing_an_undeclared_knowledge_base_is_not_found_rather_than_unauthorized() {
    // Given — an undeclared base does not exist for anyone, whatever they hold.
    let app = app(notes([signed_in_everything()])).await;

    // When
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/nope")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
