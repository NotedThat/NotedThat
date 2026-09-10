use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::fixture::app;

/// Every route, and the method that reaches it.
///
/// Authorization moved out of the middleware and into the handlers when rules
/// became path-scoped, which means a route added later without an `allows` call
/// would be world-readable. This is the compensating control, and it is
/// load-bearing rather than incidental: extending the API means extending this
/// list, and a route that forgets its check fails here instead of shipping.
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
async fn every_route_refuses_a_principal_holding_no_grant() {
    // Given — `notes` is declared, but its policy grants nothing to anyone.
    let policies = BTreeMap::from([("notes".to_string(), notedthat_core::AccessPolicy::empty())]);

    // When / Then
    for (method, uri) in EVERY_ROUTE {
        let response = app(policies.clone())
            .await
            .oneshot(
                Request::builder()
                    .method(*method)
                    .uri(*uri)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .expect("request"),
            )
            .await
            .expect("response");

        // Two refusals are correct here, from two different layers, and which
        // one a route gives is not what this asserts. The middleware answers
        // `401` for a `(method, route)` pair absent from `ANONYMOUS_REACHABLE`;
        // a handler that was reached answers `404`, because an anonymous denial
        // must not be distinguishable from an undeclared slug. What must never
        // happen is the request succeeding.
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
