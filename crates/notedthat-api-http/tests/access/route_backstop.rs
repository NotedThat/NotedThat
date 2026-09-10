use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::fixture::{TOKEN, app};

/// Every route, and the method that reaches it.
///
/// Authorization moved out of the middleware and into the handlers when rules
/// became path-scoped, which means a route added later without an `allows` call
/// would be world-readable. This is the compensating control, and it is
/// load-bearing rather than incidental: extending the API means extending this
/// list, and a route that forgets its check fails here instead of shipping.
///
/// The list is walked once per principal, and it takes both passes to make that
/// promise hold. The anonymous pass is largely the middleware's answer — a
/// `(method, route)` pair absent from `ANONYMOUS_REACHABLE` never reaches a
/// handler at all, so those assertions pass whether or not the handler checks
/// anything. Only the signed-in pass depends on the handler's own `KbAccess`
/// call, and that is the case path-scoped rules exist for: a route added
/// without one would be shut to the world and open to every token holder.
const EVERY_ROUTE: &[(&str, &str)] = &[
    ("GET", "/api/v1/knowledgebases"),
    ("HEAD", "/api/v1/knowledgebases"),
    ("GET", "/api/v1/knowledgebases/notes"),
    ("HEAD", "/api/v1/knowledgebases/notes"),
    ("GET", "/api/v1/knowledgebases/notes/public.md"),
    ("HEAD", "/api/v1/knowledgebases/notes/public.md"),
    ("PUT", "/api/v1/knowledgebases/notes/public.md"),
    ("DELETE", "/api/v1/knowledgebases/notes/public.md"),
    ("PATCH", "/api/v1/knowledgebases/notes/public.md"),
    ("POST", "/api/v1/knowledgebases/notes/replace/public.md"),
    ("POST", "/api/v1/knowledgebases/notes/search"),
];

#[tokio::test]
async fn every_route_refuses_an_anonymous_principal_holding_no_grant() {
    // Given — `notes` is declared, but its policy grants nothing to anyone.
    let policies = BTreeMap::from([("notes".to_string(), notedthat_core::AccessPolicy::empty())]);

    // When / Then — two refusals are correct here, from two different layers,
    // and which one a route gives is not what this pass is asserting. The
    // middleware answers `401` for a `(method, route)` pair absent from
    // `ANONYMOUS_REACHABLE`; a handler that was reached answers `404`, because
    // an anonymous denial must not be distinguishable from an undeclared slug.
    // What must never happen is the request succeeding.
    for (method, uri) in EVERY_ROUTE {
        let response = request(&policies, method, uri, None).await;

        assert!(
            matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND
            ),
            "{method} {uri} answered {} — an ungranted anonymous caller got through",
            response.status()
        );
    }
}

#[tokio::test]
async fn every_route_refuses_a_credentialed_principal_holding_no_grant() {
    // Given — the same grantless policy, asked by a valid credential. The
    // middleware lets every one of these through, so the refusal can only come
    // from the handler's own authorization call.
    let policies = BTreeMap::from([("notes".to_string(), notedthat_core::AccessPolicy::empty())]);

    // When / Then
    for (method, uri) in EVERY_ROUTE {
        let response = request(&policies, method, uri, Some(TOKEN)).await;

        if INDEX_ROUTES.contains(&(method, uri)) {
            // The knowledge-base index is not key-scoped and has no verb to
            // check: the credential holder can always reach `.notedthat` in
            // every declared knowledge base, so they are listed. What it must
            // not do is disclose anything beyond the slugs it is asked for.
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{method} {uri} refused the credential holder its own index"
            );
            continue;
        }

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} let an ungranted credentialed caller through — \
             the handler is missing its `KbAccess` check"
        );
    }
}

/// The routes that answer a knowledge-base-wide question rather than a
/// key-scoped one, and so cannot answer `403` on an empty policy.
const INDEX_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/v1/knowledgebases"),
    ("HEAD", "/api/v1/knowledgebases"),
];

async fn request(
    policies: &BTreeMap<String, notedthat_core::AccessPolicy>,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }

    app(policies.clone())
        .await
        .oneshot(builder.body(Body::from("{}")).expect("request"))
        .await
        .expect("response")
}
