//! Static-Bearer authentication middleware for the `NotedThat` API.

use crate::error::ApiErrorResponse;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use notedthat_core::{extract_bearer_from_header, verify_bearer_token};
use tower_http::request_id::RequestId;

/// Health-check paths that bypass Bearer authentication.
const AUTH_EXEMPT_PATHS: &[&str] = &["/healthz", "/readyz"];

/// Axum middleware that validates the `Authorization: Bearer <token>` header.
///
/// Requests to health-check paths pass through without authentication.
/// All other requests must present a valid Bearer token that matches
/// `state.bearer_token` (compared in constant time).
pub async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiErrorResponse> {
    let path = req.uri().path();
    if AUTH_EXEMPT_PATHS.contains(&path) {
        return Ok(next.run(req).await);
    }

    let request_id = extract_request_id(&req);

    let header_value = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let Some(token) = header_value.and_then(extract_bearer_from_header) else {
        return Err(ApiErrorResponse::unauthorized(request_id));
    };

    if !verify_bearer_token(token, &state.bearer_token) {
        return Err(ApiErrorResponse::unauthorized(request_id));
    }

    Ok(next.run(req).await)
}

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
