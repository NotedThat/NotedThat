//! Bearer token authentication middleware for the MCP HTTP endpoint.
//!
//! Provides [`require_bearer_auth`], an axum middleware function that resolves
//! the caller through the shared [`Authenticator`] and refuses anything that
//! is not a verified bearer credential.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::WWW_AUTHENTICATE},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use notedthat_core::{Authenticator, Schemes, extract_bearer_from_header};
use serde::Serialize;
use std::sync::Arc;

/// The bearer the MCP caller presented, verbatim, for the tools to act with.
///
/// The MCP service is an HTTP client of this server's own API. Forwarding the
/// caller's credential rather than the server's is what makes MCP act as the
/// calling identity, so a `group:` rule binds a tool call exactly as it binds
/// a direct request. Stored in the request extensions by
/// [`require_bearer_auth`]; rmcp carries those into every tool call.
#[derive(Clone)]
pub struct CallerToken(String);

impl CallerToken {
    /// The bearer value, for `Authorization: Bearer …` on the loopback call.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A token the middleware did not vouch for, for tests of what the tools
    /// do with one.
    #[cfg(test)]
    pub(crate) fn unverified(token: &str) -> Self {
        Self(token.to_string())
    }
}

impl std::fmt::Debug for CallerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallerToken(<redacted>)")
    }
}

/// JSON body returned with every 401 Unauthorized response.
#[derive(Debug, Serialize)]
struct UnauthorizedBody {
    error: &'static str,
    message: &'static str,
}

/// Axum middleware that enforces Bearer token authentication on the MCP HTTP endpoint.
///
/// Reads the `Authorization` header from the incoming request and resolves it
/// through the [`Authenticator`]: the service token or a bearer token an
/// identity provider vouches for.
///
/// Returns **HTTP 401** with a JSON error body for any of:
/// - missing `Authorization` header — MCP has no anonymous mode
/// - wrong authentication scheme (e.g. Basic, not Bearer)
/// - a token that is neither the service token nor verifiable
///
/// The `401` carries a `WWW-Authenticate` bearer challenge naming the
/// protected-resource metadata when the deployment publishes it, which is how
/// an MCP client discovers where to obtain a token.
///
/// On success the resolved [`notedthat_core::Principal`] and the caller's
/// [`CallerToken`] are stored in the request extensions and the request is
/// forwarded to the next handler.
///
/// # Usage
///
/// ```rust,ignore
/// use axum::{Router, middleware};
/// use notedthat_mcp::auth::require_bearer_auth;
///
/// let authenticator = Arc::new(Authenticator::new("super-secret"));
/// let router = Router::new()
///     .route("/mcp", /* mcp service */)
///     .layer(middleware::from_fn_with_state(authenticator, require_bearer_auth));
/// ```
pub async fn require_bearer_auth(
    State(authenticator): State<Arc<Authenticator>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    match authenticator
        .resolve(request.headers(), Schemes::Bearer)
        .await
    {
        Ok(principal) if principal.is_signed_in() => {
            // `resolve` accepted exactly one Bearer header, so this is it.
            let token = request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(extract_bearer_from_header)
                .map(|token| CallerToken(token.to_string()));
            if let Some(token) = token {
                request.extensions_mut().insert(token);
            }
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        _ => {
            let mut response = (
                StatusCode::UNAUTHORIZED,
                Json(UnauthorizedBody {
                    error: "unauthorized",
                    message: "missing or invalid Authorization header",
                }),
            )
                .into_response();
            if let Some(challenge) = authenticator.bearer_challenge() {
                response.headers_mut().insert(WWW_AUTHENTICATE, challenge);
            }
            response
        }
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
                Arc::new(Authenticator::new(token)),
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

    #[tokio::test]
    async fn a_verified_identity_token_passes_through() {
        // Given: an authenticator with a verifier vouching for one token.
        let authenticator = Arc::new(Authenticator::new("secret").with_token_verifier(Arc::new(
            notedthat_core::testing::StubTokenVerifier::default().accepting(
                "jwt-alice",
                "alice",
                [],
            ),
        )));
        let app =
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(middleware::from_fn_with_state(
                    authenticator,
                    require_bearer_auth,
                ));
        let req = Request::builder()
            .uri("/")
            .header("Authorization", "Bearer jwt-alice")
            .body(Body::empty())
            .unwrap();

        // When / Then
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_401_keeps_its_json_body_and_adds_the_challenge_when_published() {
        // Given
        let metadata_url = "https://notes.example.com/.well-known/oauth-protected-resource";
        let authenticator = Arc::new(Authenticator::new("secret").with_protected_resource(
            notedthat_core::ProtectedResource {
                resource: "https://notes.example.com".into(),
                authorization_servers: vec!["https://auth.example.com".into()],
                metadata_url: metadata_url.into(),
            },
        ));
        let app =
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(middleware::from_fn_with_state(
                    authenticator,
                    require_bearer_auth,
                ));

        // When
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        // Then
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers().get("www-authenticate").unwrap(),
            &format!("Bearer resource_metadata=\"{metadata_url}\"")
        );
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "unauthorized");
    }
}
