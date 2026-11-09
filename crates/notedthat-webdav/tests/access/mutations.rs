use notedthat_core::testing::StorageOp;
use notedthat_core::{AccessPolicy, AccessRule, KeyPattern, Verb, Who};
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_webdav::router::build_router;
use tower::ServiceExt;

use super::fixture::{PROPFIND_BODY, policy, request, response_body, state_with_policies};
use super::storage::memory_storage;

#[tokio::test]
async fn anonymous_mutations_are_challenged_before_storage() {
    // Given
    let storage = Arc::new(memory_storage([("discoverable", "public.md")]).await);
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(Who::Anyone, &[Verb::List, Verb::Read]),
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
    assert_eq!(storage.ops().len(), 0, "calls: {:?}", storage.ops());
}

#[tokio::test]
async fn real_router_manual_read_qa_scenario() {
    // Given
    let storage = Arc::new(
        memory_storage([
            ("discoverable", ".notedthat/manifest.json"),
            ("discoverable", "public.md"),
        ])
        .await,
    );
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(Who::Anyone, &[Verb::List, Verb::Read]),
        )]),
    ));

    // When
    let root = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PROPFIND")
                .uri("/webdav")
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
                .uri("/webdav/discoverable/public.md")
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
    let storage = Arc::new(
        memory_storage([
            ("discoverable", ".notedthat/manifest.json"),
            ("discoverable", "public.md"),
        ])
        .await,
    );
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        BTreeMap::from([(
            "discoverable".to_string(),
            policy(Who::Anyone, &[Verb::List, Verb::Read]),
        )]),
    ));

    // When
    let bad_basic = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/webdav/discoverable/public.md")
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
                .uri("/webdav/discoverable/public.md")
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
    assert!(!storage.ops().iter().any(|op| {
        matches!(
            op,
            StorageOp::PutObject
                | StorageOp::PutStagedObject
                | StorageOp::DeleteObject
                | StorageOp::CopyObject
        )
    }));
}

/// Write under `drafts/**`, read-only under `published/**` (#441).
fn drafts_writable_published_read_only() -> BTreeMap<String, AccessPolicy> {
    let pattern = |source| KeyPattern::parse(source).expect("valid pattern");
    BTreeMap::from([(
        "discoverable".to_string(),
        [
            AccessRule::new(
                Who::SignedIn,
                [Verb::List, Verb::Read, Verb::Write, Verb::Delete],
            )
            .under([pattern("drafts/**")]),
            AccessRule::new(Who::SignedIn, [Verb::List, Verb::Read])
                .under([pattern("published/**")]),
        ]
        .into_iter()
        .collect(),
    )])
}

fn signed_in_transfer(method: &str, from: &str, to: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/webdav/discoverable/{from}"))
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .header("Host", "localhost")
        .header("Destination", format!("/webdav/discoverable/{to}"))
        .body(Body::empty())
        .expect("valid request")
}

#[tokio::test]
async fn copy_and_move_into_a_read_only_prefix_are_forbidden_before_storage() {
    for method in ["MOVE", "COPY"] {
        // Given
        let storage = Arc::new(memory_storage([("discoverable", "drafts/a.md")]).await);
        let app = build_router(state_with_policies(
            Arc::clone(&storage),
            drafts_writable_published_read_only(),
        ));

        // When
        let response = app
            .oneshot(signed_in_transfer(method, "drafts/a.md", "published/a.md"))
            .await
            .expect("router response");

        // Then
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "method={method}");
        assert!(
            storage.ops().is_empty(),
            "method={method} calls: {:?}",
            storage.ops()
        );
    }
}

#[tokio::test]
async fn move_within_a_writable_prefix_still_succeeds() {
    // Given
    let storage = Arc::new(memory_storage([("discoverable", "drafts/a.md")]).await);
    let app = build_router(state_with_policies(
        Arc::clone(&storage),
        drafts_writable_published_read_only(),
    ));

    // When
    let response = app
        .oneshot(signed_in_transfer("MOVE", "drafts/a.md", "drafts/b.md"))
        .await
        .expect("router response");

    // Then: authorized, so the copy reaches storage. The fixture's index queue has no
    // receiver, so the request ends in 503 after the copy; that is not what is pinned.
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(storage.count(StorageOp::CopyObject), 1);
}
