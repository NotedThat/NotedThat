//! Authentication middleware for the `NotedThat` API.

use crate::error::ApiErrorResponse;
use crate::router::{
    MATCHED_KB, MATCHED_KB_EVENTS, MATCHED_KB_OBJECT, MATCHED_KB_SEARCH, MATCHED_KBS,
};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{MatchedPath, State};
use axum::http::{Method, Request, StatusCode, header::WWW_AUTHENTICATE};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use notedthat_core::{Principal, Schemes};
use tower_http::request_id::RequestId;

/// The `(method, route)` pairs an anonymous request is allowed to reach.
///
/// Authorization itself is per key and lives in the handlers, because a
/// path-scoped rule cannot be evaluated from a route pattern — `read` on
/// `{*object_path}` has no answer until the key is known. That would leave a
/// route added later without an authorization call open to the world, so the
/// table is inverted instead of deleted: a `(method, route)` pair absent from
/// here is unreachable without a credential, and opening a new route means
/// coming here and saying so.
///
/// Every route listed **must** have a handler that calls
/// [`crate::authz::KbAccess::require`] or `require_any`; `route_backstop.rs`
/// asserts the two stay in step.
const ANONYMOUS_REACHABLE: &[(&Method, &str)] = &[
    (&Method::GET, MATCHED_KBS),
    (&Method::HEAD, MATCHED_KBS),
    (&Method::GET, MATCHED_KB),
    (&Method::HEAD, MATCHED_KB),
    (&Method::GET, MATCHED_KB_OBJECT),
    (&Method::HEAD, MATCHED_KB_OBJECT),
    (&Method::POST, MATCHED_KB_SEARCH),
    (&Method::GET, MATCHED_KB_EVENTS),
];

/// Axum middleware that establishes the request's [`Principal`].
///
/// This layer authenticates; it does not authorize. A credential the
/// [`notedthat_core::Authenticator`] accepts makes the request
/// [`Principal::SignedIn`], an absent credential makes it [`Principal::Anyone`],
/// and a supplied credential that does not verify is always `401` — never
/// quietly downgraded to anonymous, which is the rule that stops a typo'd token
/// from silently becoming a public view.
///
/// Every `401` that leaves this layer — its own, or one a handler answered —
/// carries the bearer challenge when the deployment publishes protected-
/// resource metadata, so an MCP client can find the authorization server.
///
/// It is mounted on the `/api/v1` routes only. The unauthenticated root routes
/// (`/healthz`, `/readyz`, `/llms.txt`) never reach it, and `/browse` resolves
/// its own principal through the same authenticator because it is mounted
/// outside this layer (see [`crate::router::browse`]).
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let request_id = extract_request_id(&req);

    let response = match state
        .authenticator
        .resolve(req.headers(), Schemes::Bearer)
        .await
    {
        Err(CredentialRefused) => ApiErrorResponse::unauthorized(request_id).into_response(),
        Ok(principal) if principal.is_anonymous() && !anonymous_may_reach(&req) => {
            ApiErrorResponse::unauthorized(request_id).into_response()
        }
        Ok(principal) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
    };
    with_bearer_challenge(&state, response)
}

/// Add the `WWW-Authenticate` challenge to a `401`, when there is one to add.
pub(crate) fn with_bearer_challenge(state: &AppState, mut response: Response) -> Response {
    if response.status() == StatusCode::UNAUTHORIZED
        && let Some(challenge) = state.authenticator.bearer_challenge()
    {
        response.headers_mut().insert(WWW_AUTHENTICATE, challenge);
    }
    response
}

/// Whether this request's route lets an anonymous caller through to a handler
/// that will authorize it per key.
fn anonymous_may_reach<B>(req: &Request<B>) -> bool {
    let Some(matched) = req.extensions().get::<MatchedPath>() else {
        return false;
    };
    let matched = matched.as_str();
    ANONYMOUS_REACHABLE
        .iter()
        .any(|(method, route)| *method == req.method() && *route == matched)
}

pub use notedthat_core::CredentialRefused;

/// The principal established at the HTTP boundary.
///
/// Defaults to [`Principal::Anyone`] when the auth layer has not run, so a
/// handler reached by an unexpected route fails closed rather than open.
pub fn principal<B>(req: &Request<B>) -> Principal {
    req.extensions()
        .get::<Principal>()
        .cloned()
        .unwrap_or(Principal::Anyone)
}

pub use notedthat_core::is_internal_path;

/// Extract the `x-request-id` value from request extensions, falling back to a
/// generated UUID if the `SetRequestId` middleware hasn't run yet.
pub fn extract_request_id<B>(req: &Request<B>) -> String {
    req.extensions()
        .get::<RequestId>()
        .and_then(|r| r.header_value().to_str().ok())
        .map_or_else(
            || {
                tracing::warn!("request_id missing from Extensions — generating fallback");
                uuid::Uuid::now_v7().to_string()
            },
            str::to_string,
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::InMemoryStorage;
    use axum::middleware::from_fn_with_state;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Router, body::Body, http::StatusCode};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    fn test_state(token: &str) -> AppState {
        let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
        AppState {
            storage: Arc::new(InMemoryStorage::default()),
            declared_kbs: Arc::new(BTreeMap::new()),
            access_policies: Arc::new(BTreeMap::new()),
            authenticator: Arc::new(notedthat_core::Authenticator::new(token)),
            max_body_size: 16 * 1024 * 1024,
            max_patchable_size: 16 * 1024 * 1024,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
            events: None,
        }
    }

    fn app(token: &str) -> Router {
        let state = test_state(token);
        Router::new()
            .route("/protected", get(|| async { "secret".into_response() }))
            .layer(from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state)
    }

    /// The public routes are exempt because they are mounted outside this
    /// layer, not because the layer knows their paths. Anything that does
    /// reach the layer without a credential is rejected — including a path
    /// that merely looks like a probe.
    #[tokio::test]
    async fn no_path_is_exempt_once_the_request_reaches_this_layer() {
        for uri in ["/healthz", "/readyz", "/llms.txt", "/protected"] {
            let resp = app("my-token")
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} must not be exempted by the auth layer itself"
            );
        }
    }

    #[tokio::test]
    async fn test_rejects_missing_auth() {
        let resp = app("my-token")
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_rejects_wrong_token() {
        let resp = app("real-token")
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_accepts_correct_token() {
        let resp = app("my-token")
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("authorization", "Bearer my-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_accepts_lowercase_bearer_scheme() {
        let resp = app("my-token")
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header("authorization", "bearer my-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
