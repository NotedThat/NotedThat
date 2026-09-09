use std::collections::BTreeMap;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use notedthat_api_http::middleware::principal;
use notedthat_core::{AccessPolicy, Principal, Verb};
use tower::ServiceExt;

use super::fixture::{TOKEN, app, grant, policy};

fn notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

#[test]
fn a_request_that_never_reached_the_auth_layer_is_anonymous() {
    // Given / When / Then — failing closed matters more here than anywhere: a
    // handler reached by an unexpected route must not inherit a credential.
    let request = Request::new(Body::empty());
    assert_eq!(principal(&request), Principal::Anyone);
}

#[tokio::test]
async fn head_follows_get_including_for_a_percent_encoded_slug() {
    // Given / When / Then
    for slug in ["notes", "%6Eotes"] {
        for method in ["GET", "HEAD"] {
            let app = app(notes([grant(Principal::Anyone, [Verb::List])])).await;
            let response = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/api/v1/knowledgebases/{slug}"))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{method} {slug}");
        }
    }
}

#[tokio::test]
async fn discovery_is_refused_when_no_declared_knowledge_base_grants_anything() {
    // Given — no policies at all, a grant on a base that is not declared, and a
    // declared base whose only grant belongs to the credential holder.
    let cases = [
        BTreeMap::new(),
        BTreeMap::from([(
            "undeclared".to_string(),
            policy([grant(Principal::Anyone, [Verb::Read])]),
        )]),
        notes([grant(Principal::SignedIn, [Verb::Read])]),
    ];

    // When / Then
    for policies in cases {
        for method in ["GET", "HEAD"] {
            let response = app(policies.clone())
                .await
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/api/v1/knowledgebases")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method}");
        }
    }
}

#[tokio::test]
async fn discovery_is_public_as_soon_as_one_knowledge_base_grants_anything() {
    // Given / When
    let response = app(notes([grant(Principal::Anyone, [Verb::Search])]))
        .await
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/api/v1/knowledgebases")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then — `search` is not a listing verb, but holding it still makes the
    // knowledge base worth naming.
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn absent_credentials_can_fall_back_but_supplied_bad_credentials_cannot() {
    // Given — the rule that stops a typo'd token from becoming a public view.
    let app = app(notes([grant(Principal::Anyone, [Verb::Read])])).await;

    // When
    let absent = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let wrong = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .header("authorization", "Bearer not-the-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let malformed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .header("authorization", "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let duplicated = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(absent.status(), StatusCode::OK);
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(malformed.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(duplicated.status(), StatusCode::UNAUTHORIZED);
}
