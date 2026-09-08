use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_webdav::router::build_router;
use tower::ServiceExt;

use super::fixture::{PROPFIND_BODY, policy, request, response_body, state_with_policies};
use super::storage::MemoryStorage;

#[tokio::test]
async fn anonymous_mutations_are_challenged_before_storage() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([("discoverable", "public.md")]));
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(&["discover", "browse", "content"]),
        )]),
    ));

    for method in [
        "PUT",
        "DELETE",
        "MOVE",
        "COPY",
        "MKCOL",
        "PROPPATCH",
        "LOCK",
        "UNLOCK",
    ] {
        // When
        let response = app
            .clone()
            .oneshot(request(method, "/discoverable/new.md"))
            .await
            .expect("router response");

        // Then
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "method={method}"
        );
    }
    let malformed_path = app
        .oneshot(request("MOVE", "/discoverable/%2e%2e/new.md"))
        .await
        .expect("malformed write response");
    assert_eq!(malformed_path.status(), StatusCode::UNAUTHORIZED);
    assert!(storage.calls().is_empty(), "calls: {:?}", storage.calls());
}

#[tokio::test]
async fn real_router_manual_read_qa_scenario() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([
        ("discoverable", ".notedthat/manifest.json"),
        ("discoverable", "public.md"),
    ]));
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(&["discover", "browse", "content"]),
        )]),
    ));

    // When
    let root = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PROPFIND")
                .uri("/")
                .header("Depth", "1")
                .body(Body::from(PROPFIND_BODY))
                .expect("valid request"),
        )
        .await
        .expect("root response");
    let root_status = root.status();
    let root_body = response_body(root).await;
    let range = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/discoverable/public.md")
                .header("Range", "bytes=0-2")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("range response");
    let range_status = range.status();
    let options = app
        .clone()
        .oneshot(request("OPTIONS", "/discoverable/public.md"))
        .await
        .expect("options response");
    let options_status = options.status();
    let _allow = options
        .headers()
        .get("allow")
        .expect("Allow header")
        .clone();

    // Then
    assert_eq!(root_status, StatusCode::MULTI_STATUS);
    assert!(root_body.contains("discoverable"));
    assert_eq!(range_status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(options_status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn real_router_manual_denial_qa_scenario() {
    // Given
    let storage = Arc::new(MemoryStorage::with_objects([
        ("discoverable", ".notedthat/manifest.json"),
        ("discoverable", "public.md"),
    ]));
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(&["discover", "browse", "content"]),
        )]),
    ));

    // When
    let bad_basic = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/discoverable/public.md")
                .header("Authorization", "Basic !!!")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("bad Basic response");
    let bad_basic_status = bad_basic.status();
    let _challenge = bad_basic
        .headers()
        .get("www-authenticate")
        .expect("challenge")
        .clone();
    let bad_basic_body = response_body(bad_basic).await;
    let internal = app
        .clone()
        .oneshot(request("GET", "/discoverable/.notedthat/manifest.json"))
        .await
        .expect("internal response");
    let malformed_destination = app
        .oneshot(
            Request::builder()
                .method("MOVE")
                .uri("/discoverable/public.md")
                .header("Destination", "http://[malformed")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("MOVE response");

    // Then
    assert_eq!(bad_basic_status, StatusCode::UNAUTHORIZED);
    assert!(bad_basic_body.contains("valid credentials are required"));
    assert_eq!(internal.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(malformed_destination.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !storage
            .calls()
            .iter()
            .any(|call| { matches!(call.as_str(), "put" | "put_staged" | "delete" | "copy") })
    );
}
