use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_webdav::router::build_router;
use tower::ServiceExt;

use super::fixture::{policy, request, state_with_policies};
use super::storage::MemoryStorage;

#[tokio::test]
async fn anonymous_options_advertises_only_capability_methods() {
    // Given
    let storage = Arc::new(MemoryStorage::default());
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([
            ("discoverable".to_string(), policy(&["browse"])),
            ("private".to_string(), policy(&["content"])),
        ]),
    ));

    // When
    let root = app
        .clone()
        .oneshot(request("OPTIONS", "/"))
        .await
        .expect("root options");
    let browse = app
        .clone()
        .oneshot(request("OPTIONS", "/discoverable/folder"))
        .await
        .expect("browse options");
    let content = app
        .oneshot(request("OPTIONS", "/private/file.md"))
        .await
        .expect("content options");

    // Then
    assert_eq!(root.status(), StatusCode::NO_CONTENT);
    assert_eq!(root.headers().get("allow").expect("Allow"), "OPTIONS");
    assert_eq!(
        browse.headers().get("allow").expect("Allow"),
        "OPTIONS, PROPFIND"
    );
    assert_eq!(
        content.headers().get("allow").expect("Allow"),
        "OPTIONS, GET, HEAD"
    );
}

#[tokio::test]
async fn anonymous_options_rejects_private_targets_and_private_root() {
    // Given
    let storage = Arc::new(MemoryStorage::default());
    let private_app = build_router(state_with_policies(Arc::clone(&storage), BTreeMap::new()));
    let mixed_app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("discoverable".to_string(), policy(&["discover"]))]),
    ));

    // When
    let private_root = private_app
        .oneshot(request("OPTIONS", "/"))
        .await
        .expect("private root options");
    let private_kb = mixed_app
        .oneshot(request("OPTIONS", "/private/file.md"))
        .await
        .expect("private KB options");

    // Then
    assert_eq!(private_root.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(private_kb.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anonymous_content_preserves_range_and_conditional_responses() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([("private", "private.md")]));
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([("private".to_string(), policy(&["content"]))]),
    ));

    // When
    let range = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/private/private.md")
                .header("Range", "bytes=0-2")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("range response");
    let conditional = app
        .oneshot(
            Request::builder()
                .uri("/private/private.md")
                .header("If-None-Match", "\"private.md\"")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("conditional response");

    // Then
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
}
