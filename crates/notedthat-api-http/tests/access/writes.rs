use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_core::{AccessPolicy, Principal, Verb};
use tower::ServiceExt;

use super::fixture::{TOKEN, app, grant, grant_under, json, policy, signed_in_everything};

fn notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

/// Every mutating route, with a body each will accept far enough to reach its
/// authorization check.
const MUTATIONS: &[(&str, &str)] = &[
    ("PUT", "/api/v1/knowledgebases/notes/new.md"),
    ("PATCH", "/api/v1/knowledgebases/notes/new.md"),
    ("DELETE", "/api/v1/knowledgebases/notes/new.md"),
    ("POST", "/api/v1/knowledgebases/notes/replace/new.md"),
];

#[tokio::test]
async fn a_supplied_credential_that_does_not_verify_is_never_treated_as_anonymous() {
    // Given
    let app = app(notes([grant(Principal::Anyone, [Verb::Read])])).await;

    // When / Then
    for authorization in ["Basic abc", "Bearer wrong", "Bearer "] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/knowledgebases/notes/public.md")
                    .header("authorization", authorization)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{authorization}"
        );
        assert_eq!(json(response).await["error"], "unauthorized");
    }

    // A header that is not even UTF-8, and a valid token alongside a bad one.
    let mut non_utf8 = Request::builder()
        .uri("/api/v1/knowledgebases/notes/public.md")
        .body(Body::empty())
        .expect("request");
    non_utf8.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_bytes(&[0xff]).expect("opaque header value"),
    );
    let non_utf8_response = app.clone().oneshot(non_utf8).await.expect("response");

    let mut duplicated = Request::builder()
        .uri("/api/v1/knowledgebases/notes/public.md")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .expect("request");
    duplicated.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let duplicated_response = app.oneshot(duplicated).await.expect("response");

    assert_eq!(non_utf8_response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(duplicated_response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anonymous_mutation_is_refused_under_the_broadest_grant_and_changes_nothing() {
    // Given — every verb an anonymous rule may legally carry.
    let app = app(notes([
        grant(Principal::Anyone, [Verb::List, Verb::Read, Verb::Search]),
        signed_in_everything(),
    ]))
    .await;

    // When
    for (method, uri) in MUTATIONS {
        let write = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(*method)
                    .uri(*uri)
                    .header("if-match", "\"abc\"")
                    .body(Body::from("new body"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(write.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    let read = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/new.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(read.status(), StatusCode::NOT_FOUND, "nothing was written");
}

#[tokio::test]
async fn a_write_grant_does_not_carry_a_delete_grant() {
    // Given — expressible for the first time: the credential holder may create
    // and amend, but not remove.
    let app = app(notes([grant(
        Principal::SignedIn,
        [Verb::List, Verb::Read, Verb::Write],
    )]))
    .await;

    // When
    let write = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/new.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("new body"))
                .expect("request"),
        )
        .await
        .expect("response");
    let delete = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/knowledgebases/notes/new.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert!(write.status().is_success(), "{:?}", write.status());
    assert_eq!(delete.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_prefix_scoped_write_grant_stops_at_its_prefix() {
    // Given
    let app = app(notes([grant_under(
        Principal::SignedIn,
        [Verb::Write],
        &["public/**"],
    )]))
    .await;

    // When
    let inside = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/public%2Fnew.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("new body"))
                .expect("request"),
        )
        .await
        .expect("response");
    let outside = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/internal%2Fnew.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("new body"))
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert!(inside.status().is_success(), "{:?}", inside.status());
    assert_eq!(outside.status(), StatusCode::FORBIDDEN);
}
