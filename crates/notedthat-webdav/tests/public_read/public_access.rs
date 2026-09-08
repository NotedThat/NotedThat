use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use notedthat_webdav::router::build_router;
use tower::ServiceExt;

use super::fixture::{PROPFIND_BODY, policy, request, response_body, state_with_policies};
use super::storage::MemoryStorage;

#[tokio::test]
async fn anonymous_root_propfind_lists_only_discoverable_kbs() {
    // Given
    let storage = Arc::new(MemoryStorage::default());
    let app = build_router(state_with_policies(
        storage,
        BTreeMap::from([
            ("discoverable".to_string(), policy(&["discover"])),
            ("private".to_string(), policy(&[])),
        ]),
    ));
    let request = Request::builder()
        .method(Method::from_bytes(b"PROPFIND").expect("valid method"))
        .uri("/")
        .header("Depth", "1")
        .body(Body::from(PROPFIND_BODY))
        .expect("valid request");

    // When
    let response = app.oneshot(request).await.expect("router response");

    // Then
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let body = response_body(response).await;
    assert!(body.contains("discoverable"), "body: {body}");
    assert!(!body.contains("private"), "body: {body}");
}

#[tokio::test]
async fn anonymous_capabilities_are_independent() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([
        ("discoverable", "public.md"),
        ("private", "private.md"),
    ]));
    let policies = BTreeMap::from([
        ("discoverable".to_string(), policy(&["browse"])),
        ("private".to_string(), policy(&["content"])),
    ]);
    let app = build_router(state_with_policies(storage, policies));

    // When
    let root = app
        .clone()
        .oneshot(request("PROPFIND", "/"))
        .await
        .expect("root response");
    let browse = app
        .clone()
        .oneshot(request("PROPFIND", "/discoverable"))
        .await
        .expect("browse response");
    let browse_content = app
        .clone()
        .oneshot(request("GET", "/discoverable/public.md"))
        .await
        .expect("browse content response");
    let content = app
        .clone()
        .oneshot(request("GET", "/private/private.md"))
        .await
        .expect("content response");
    let content_head = app
        .clone()
        .oneshot(request("HEAD", "/private/private.md"))
        .await
        .expect("content HEAD response");
    let content_browse = app
        .oneshot(request("PROPFIND", "/private"))
        .await
        .expect("content browse response");

    // Then
    assert_eq!(root.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(browse.status(), StatusCode::MULTI_STATUS);
    assert_eq!(browse_content.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(content.status(), StatusCode::OK);
    assert_eq!(content_head.status(), StatusCode::OK);
    assert_eq!(content_browse.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anonymous_internal_paths_are_challenged_and_filtered_across_pages() {
    // Given
    let storage = Arc::new(
        MemoryStorage::with_objects([
            ("discoverable", ".notedthat/manifest.json"),
            ("discoverable", "public.md"),
        ])
        .with_page_size(1),
    );
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([("discoverable".to_string(), policy(&["browse", "content"]))]),
    ));

    // When
    let listing = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PROPFIND")
                .uri("/discoverable")
                .header("Depth", "1")
                .body(Body::from(PROPFIND_BODY))
                .expect("valid request"),
        )
        .await
        .expect("listing response");
    let internal = app
        .oneshot(request("GET", "/discoverable/.notedthat/manifest.json"))
        .await
        .expect("internal response");

    // Then
    assert_eq!(listing.status(), StatusCode::MULTI_STATUS);
    let body = response_body(listing).await;
    assert!(body.contains("public.md"), "body: {body}");
    assert!(!body.contains(".notedthat"), "body: {body}");
    assert_eq!(internal.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        storage
            .calls()
            .iter()
            .filter(|call| *call == "list")
            .count(),
        2
    );
}

#[tokio::test]
async fn authenticated_access_remains_unfiltered() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([(
        "private",
        ".notedthat/manifest.json",
    )]));
    let app = build_router(state_with_policies(storage, BTreeMap::new()));

    // When
    let root = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PROPFIND")
                .uri("/")
                .header("Authorization", "Basic dXNlcjpwYXNz")
                .header("Depth", "1")
                .body(Body::from(PROPFIND_BODY))
                .expect("valid request"),
        )
        .await
        .expect("root response");
    let internal = app
        .oneshot(
            Request::builder()
                .uri("/private/.notedthat/manifest.json")
                .header("Authorization", "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("internal response");

    // Then
    assert_eq!(root.status(), StatusCode::MULTI_STATUS);
    assert!(response_body(root).await.contains("private"));
    assert_eq!(internal.status(), StatusCode::OK);
}
