//! Static-Bearer authentication middleware for the `NotedThat` API.

use crate::error::ApiErrorResponse;
use crate::state::AppState;
use axum::RequestExt;
use axum::body::Body;
use axum::extract::{MatchedPath, Path, State};
use axum::http::{Method, Request, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::Response;
use notedthat_core::{PublicReadCapability, extract_bearer_from_header, verify_bearer_token};
use tower_http::request_id::RequestId;

/// Root-level paths that bypass Bearer authentication.
const AUTH_EXEMPT_PATHS: &[&str] = &["/healthz", "/readyz", "/llms.txt"];

/// Authentication state established at the HTTP boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthContext {
    /// A valid Bearer token was supplied.
    Authenticated,
    /// No credentials were supplied and a public route capability allowed access.
    Anonymous,
}

impl AuthContext {
    /// Return whether the request is using anonymous public-read access.
    #[must_use]
    pub const fn is_anonymous(self) -> bool {
        matches!(self, Self::Anonymous)
    }
}

/// Axum middleware that validates the `Authorization: Bearer <token>` header.
///
/// A supplied credential must always be a single valid Bearer token. When no
/// credential is supplied, only root public paths and explicitly granted read
/// capabilities pass through as anonymous requests.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);

    let mut authorization_values = req.headers().get_all(AUTHORIZATION).iter();
    if let Some(header) = authorization_values.next() {
        if authorization_values.next().is_some() {
            return Err(ApiErrorResponse::unauthorized(request_id));
        }
        header
            .to_str()
            .ok()
            .and_then(extract_bearer_from_header)
            .filter(|token| verify_bearer_token(token, &state.bearer_token))
            .ok_or_else(|| ApiErrorResponse::unauthorized(request_id.clone()))?;
        req.extensions_mut().insert(AuthContext::Authenticated);
        return Ok(next.run(req).await);
    }

    if anonymous_capability(&mut req, &state).await.is_some()
        || AUTH_EXEMPT_PATHS.contains(&req.uri().path())
    {
        req.extensions_mut().insert(AuthContext::Anonymous);
        return Ok(next.run(req).await);
    }

    Err(ApiErrorResponse::unauthorized(request_id))
}

async fn anonymous_capability(
    req: &mut Request<Body>,
    state: &AppState,
) -> Option<PublicReadCapability> {
    let matched_path = req.extensions().get::<MatchedPath>()?.as_str();
    let capability = match (req.method(), matched_path) {
        (&Method::GET | &Method::HEAD, "/api/v1/knowledgebases") => {
            return state
                .declared_kbs
                .keys()
                .any(|slug| {
                    state
                        .public_read_policies
                        .get(slug)
                        .is_some_and(|policy| policy.allows(PublicReadCapability::Discover))
                })
                .then_some(PublicReadCapability::Discover);
        }
        (&Method::GET | &Method::HEAD, "/api/v1/knowledgebases/{kb_slug}") => {
            PublicReadCapability::Browse
        }
        (&Method::GET | &Method::HEAD, "/api/v1/knowledgebases/{kb_slug}/{*object_path}") => {
            PublicReadCapability::Content
        }
        (&Method::POST, "/api/v1/knowledgebases/{kb_slug}/search") => PublicReadCapability::Search,
        _ => return None,
    };
    let Path(params) = req
        .extract_parts::<Path<std::collections::BTreeMap<String, String>>>()
        .await
        .ok()?;
    let kb_slug = params.get("kb_slug")?;
    state
        .public_read_policies
        .get(kb_slug)
        .filter(|policy| policy.allows(capability))
        .map(|_| capability)
}

/// Return the request authentication context, defaulting to anonymous when the auth layer has not run.
pub fn auth_context<B>(req: &Request<B>) -> AuthContext {
    req.extensions()
        .get::<AuthContext>()
        .copied()
        .unwrap_or(AuthContext::Anonymous)
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
            public_read_policies: Arc::new(BTreeMap::new()),
            bearer_token: Arc::new(token.to_string()),
            max_body_size: 16 * 1024 * 1024,
            max_patchable_size: 16 * 1024 * 1024,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        }
    }

    fn app(token: &str) -> Router {
        let state = test_state(token);
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/protected", get(|| async { "secret".into_response() }))
            .layer(from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_healthz_bypasses_auth() {
        let resp = app("my-token")
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
