use std::collections::BTreeMap;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use notedthat_api_http::middleware::principal;
use notedthat_core::{AccessPolicy, Principal, Verb, Who};
use tower::ServiceExt;

use super::fixture::{
    ALICE_TOKEN, BOB_TOKEN, TOKEN, app, app_with_authenticator, authenticator, grant, grant_under,
    json, policy,
};

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
            let app = app(notes([grant(Who::Anyone, [Verb::List])])).await;
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
            policy([grant(Who::Anyone, [Verb::Read])]),
        )]),
        notes([grant(Who::SignedIn, [Verb::Read])]),
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
    let response = app(notes([grant(Who::Anyone, [Verb::Search])]))
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
    let app = app(notes([grant(Who::Anyone, [Verb::Read])])).await;

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

async fn get(app: axum::Router, uri: &str, token: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request"),
    )
    .await
    .expect("response")
}

#[tokio::test]
async fn a_verified_identity_token_is_signed_in_and_bound_by_group_rules() {
    // Given — editors may read `public/`; everyone signed in may list it.
    let app = app(notes([
        grant_under(Who::SignedIn, [Verb::List], &["public/**"]),
        grant_under(Who::Group("editors".into()), [Verb::Read], &["public/**"]),
    ]))
    .await;

    // When
    let alice_reads = get(
        app.clone(),
        "/api/v1/knowledgebases/notes/public%2Findex.md",
        ALICE_TOKEN,
    )
    .await;
    let bob_reads = get(
        app.clone(),
        "/api/v1/knowledgebases/notes/public%2Findex.md",
        BOB_TOKEN,
    )
    .await;
    let bob_lists = get(app.clone(), "/api/v1/knowledgebases/notes", BOB_TOKEN).await;
    let alice_outside = get(
        app.clone(),
        "/api/v1/knowledgebases/notes/internal%2Fsecret.md",
        ALICE_TOKEN,
    )
    .await;

    // Then
    assert_eq!(alice_reads.status(), StatusCode::OK);
    assert_eq!(bob_reads.status(), StatusCode::FORBIDDEN);
    assert_eq!(bob_lists.status(), StatusCode::OK);
    assert_eq!(alice_outside.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_identity_denied_by_the_rules_gets_403_not_the_concealed_404() {
    // Given — a base that grants nothing to anyone.
    let app = app(notes([])).await;

    // When
    let response = get(app, "/api/v1/knowledgebases/notes/public.md", BOB_TOKEN).await;

    // Then — the credential verified, so there is no slug to conceal from them.
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(response).await["error"], "forbidden");
}

#[tokio::test]
async fn an_identity_never_reaches_the_internal_namespace() {
    // Given — the broadest grant there is.
    let app = app(notes([grant(Who::SignedIn, Verb::ALL)])).await;

    // When
    let alice = get(
        app.clone(),
        "/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json",
        ALICE_TOKEN,
    )
    .await;
    let service = get(
        app,
        "/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json",
        TOKEN,
    )
    .await;

    // Then
    assert_eq!(alice.status(), StatusCode::FORBIDDEN);
    assert_eq!(service.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_user_rule_matches_the_subject_and_not_the_service_token() {
    // Given
    let app = app(notes([grant(Who::User("alice".into()), [Verb::Read])])).await;

    // When / Then
    assert_eq!(
        get(
            app.clone(),
            "/api/v1/knowledgebases/notes/public.md",
            ALICE_TOKEN
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        get(app, "/api/v1/knowledgebases/notes/public.md", TOKEN)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_401_carries_the_resource_metadata_challenge_when_configured() {
    // Given
    let metadata_url = "https://notes.example.com/.well-known/oauth-protected-resource";
    let published = app_with_authenticator(
        notes([grant(Who::SignedIn, Verb::ALL)]),
        authenticator().with_protected_resource(notedthat_core::ProtectedResource {
            resource: "https://notes.example.com".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            metadata_url: metadata_url.into(),
        }),
    )
    .await;
    let bare = app(notes([grant(Who::SignedIn, Verb::ALL)])).await;

    // When — a refused credential (middleware) and an anonymous mutation
    // (middleware) and an anonymous empty index (handler) are all 401s.
    let refused = get(
        published.clone(),
        "/api/v1/knowledgebases/notes/public.md",
        "not-a-token",
    )
    .await;
    let anonymous_index = published
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let bare_refused = get(
        bare,
        "/api/v1/knowledgebases/notes/public.md",
        "not-a-token",
    )
    .await;

    // Then
    let expected = format!("Bearer resource_metadata=\"{metadata_url}\"");
    for response in [&refused, &anonymous_index] {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get("www-authenticate")
                .expect("challenge"),
            expected.as_str()
        );
    }
    assert_eq!(bare_refused.status(), StatusCode::UNAUTHORIZED);
    assert!(
        bare_refused.headers().get("www-authenticate").is_none(),
        "no challenge without published metadata"
    );
}
