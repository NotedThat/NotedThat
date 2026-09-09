use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_core::{Principal, Verb};
use tower::ServiceExt;

use super::fixture::{TOKEN, app, grant, grant_under, json, policy, signed_in_everything};

fn only_notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, notedthat_core::AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

#[tokio::test]
async fn a_knowledge_base_is_listed_when_the_caller_holds_any_grant_in_it() {
    // Given — `notes` grants anonymous reads; `private` has no policy at all.
    let app = app(only_notes([grant(Principal::Anyone, [Verb::Read])])).await;

    // When
    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let authenticated = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(anonymous.status(), StatusCode::OK);
    assert_eq!(
        json(anonymous).await["knowledgebases"],
        serde_json::json!(["notes"]),
        "a knowledge base with no anonymous grant must not be named"
    );
    assert_eq!(authenticated.status(), StatusCode::OK);
    assert_eq!(
        json(authenticated).await["knowledgebases"],
        serde_json::json!(["notes", "private"]),
        "the credential holder can always find a base to repair its manifest"
    );
}

#[tokio::test]
async fn a_read_grant_serves_get_and_head_but_never_the_internal_namespace() {
    // Given
    let app = app(only_notes([grant(Principal::Anyone, [Verb::Read])])).await;

    // When
    let get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
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
                .uri("/api/v1/knowledgebases/notes/public.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let internal = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json")
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

#[tokio::test]
async fn a_prefix_scoped_read_grant_serves_only_keys_under_that_prefix() {
    // Given — the capability model could not express this at all.
    let app = app(only_notes([grant_under(
        Principal::Anyone,
        [Verb::Read],
        &["public/**"],
    )]))
    .await;

    // When
    let inside = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public%2Fdeep%2Fnote.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let outside = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/internal%2Fsecret.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(inside.status(), StatusCode::OK);
    assert_eq!(outside.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_list_grant_does_not_imply_a_read_grant() {
    // Given
    let app = app(only_notes([grant(Principal::Anyone, [Verb::List])])).await;

    // When
    let listing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let read = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(listing.status(), StatusCode::OK);
    assert_eq!(read.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_restricted_credential_is_refused_with_forbidden_rather_than_unauthorized() {
    // Given — new with D50: the rules bind the credential holder too. The status
    // has to differ from the anonymous case, because a 401 would invite the
    // caller to retry with credentials they already sent.
    let app = app(only_notes([grant_under(
        Principal::SignedIn,
        [Verb::Read],
        &["public/**"],
    )]))
    .await;

    // When
    let granted = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public%2Findex.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let denied = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/internal%2Fsecret.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(granted.status(), StatusCode::OK);
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(denied).await["error"], "forbidden");
}

#[tokio::test]
async fn the_credential_holder_can_always_rewrite_the_manifest_that_locked_it_out() {
    // Given — a policy that grants the credential holder nothing at all. This is
    // the lockout D50 makes possible, and the recovery path has to survive it.
    let app = app(only_notes([])).await;

    // When
    let read_manifest = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let write_manifest = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await
        .expect("response");
    let ordinary_object = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(read_manifest.status(), StatusCode::OK);
    assert!(
        write_manifest.status().is_success(),
        "repair must be possible through the API, not only through the bucket"
    );
    assert_eq!(
        ordinary_object.status(),
        StatusCode::FORBIDDEN,
        "the valve is the internal namespace only; everything else stays denied"
    );
}

#[tokio::test]
async fn an_anonymous_caller_never_reaches_the_internal_namespace_under_any_grant() {
    // Given — every anonymous verb validation permits, across the whole base.
    let app = app(only_notes([
        grant(Principal::Anyone, [Verb::List, Verb::Read, Verb::Search]),
        signed_in_everything(),
    ]))
    .await;

    // When
    let read = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let listing = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(read.status(), StatusCode::UNAUTHORIZED);
    let keys = super::fixture::listed_keys(listing).await;
    assert!(
        !keys.iter().any(|key| key.starts_with(".notedthat")),
        "the internal namespace must not appear in an anonymous listing: {keys:?}"
    );
}
