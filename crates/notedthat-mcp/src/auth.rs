//! Bearer token authentication middleware for the MCP HTTP endpoint.
//!
//! Provides [`require_bearer_auth`], an axum middleware function that enforces
//! constant-time Bearer token verification using [`notedthat_core::auth`]
//! primitives per RFC 6750 and SPEC D21.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use notedthat_core::auth::{extract_bearer_from_header, verify_bearer_token};
use serde::Serialize;

/// JSON body returned with every 401 Unauthorized response.
#[derive(Debug, Serialize)]
struct UnauthorizedBody {
    error: &'static str,
    message: &'static str,
}

/// Axum middleware that enforces Bearer token authentication on the MCP HTTP endpoint.
///
/// Reads the `Authorization` header from the incoming request, extracts the
/// Bearer scheme token via [`notedthat_core::auth::extract_bearer_from_header`],
/// and verifies it against the configured `expected_token` in constant time via
/// [`notedthat_core::auth::verify_bearer_token`].
///
/// Returns **HTTP 401** with a JSON error body for any of:
/// - missing `Authorization` header
/// - wrong authentication scheme (e.g. Basic, not Bearer)
/// - non-matching token value
///
/// On success the request is forwarded unchanged to the next handler.
///
/// # Usage
///
/// ```rust,ignore
/// use axum::{Router, middleware};
/// use notedthat_mcp::auth::require_bearer_auth;
///
/// let token = "super-secret".to_string();
/// let router = Router::new()
///     .route("/mcp", /* mcp service */)
///     .layer(middleware::from_fn_with_state(token, require_bearer_auth));
/// ```
pub async fn require_bearer_auth(
    State(expected_token): State<String>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let authorized = match auth_header {
        Some(value) => match extract_bearer_from_header(value) {
            Some(provided) => verify_bearer_token(provided, &expected_token),
            None => false,
        },
        None => false,
    };

    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(UnauthorizedBody {
                error: "unauthorized",
                message: "missing or invalid Authorization header",
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod mcp_http_auth {
    use super::*;
    use axum::{Router, middleware, routing::get};
    use tower::ServiceExt as _;

    fn app(token: &str) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                token.to_string(),
                require_bearer_auth,
            ))
    }

    #[tokio::test]
    async fn missing_authorization_returns_401() {
        // Given: a request with no Authorization header.
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();

        // When: the middleware processes the request.
        let res = app("secret").oneshot(req).await.unwrap();

        // Then: 401 Unauthorized is returned.
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_bearer_token_returns_401() {
        // Given: a request with a Bearer token that does not match the expected value.
        // Use same length as "secret" (6 chars) to exercise the constant-time path.
        let req = Request::builder()
            .uri("/")
            .header("Authorization", "Bearer sec-et")
            .body(Body::empty())
            .unwrap();

        // When: the middleware processes the request.
        let res = app("secret").oneshot(req).await.unwrap();

        // Then: 401 Unauthorized is returned.
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_bearer_token_passes_through() {
        // Given: a request with the correct Bearer token.
        let req = Request::builder()
            .uri("/")
            .header("Authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();

        // When: the middleware processes the request.
        let res = app("secret").oneshot(req).await.unwrap();

        // Then: 200 OK — the request reached the inner handler.
        assert_eq!(res.status(), StatusCode::OK);
    }
}
