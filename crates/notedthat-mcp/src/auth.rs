//! Caller authentication middleware for the MCP HTTP endpoint.
//!
//! Provides [`authenticate_caller`], an axum middleware function that resolves
//! the caller through the shared [`Authenticator`]: a verified bearer is the
//! caller, a missing credential is the anonymous caller where the deployment
//! admits one, and a credential that does not verify is refused.

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

/// Who the MCP caller is, for the tools to act as on the loopback API call.
///
/// The MCP service is an HTTP client of this server's own API. Forwarding the
/// caller's credential rather than the server's — or forwarding none — is what
/// makes MCP act as the calling identity, so an access rule binds a tool call
/// exactly as it binds a direct request: a `group:` rule for a user, an
/// `anyone` rule for a caller who presented nothing. Stored in the request
/// extensions by [`authenticate_caller`]; rmcp carries those into every tool
/// call.
#[derive(Clone)]
pub enum Caller {
    /// A bearer the middleware verified, verbatim, for
    /// `Authorization: Bearer …` on the loopback call.
    Bearer(String),
    /// No credential. The loopback call carries no `Authorization` header and
    /// the API resolves it to the anonymous principal.
    Anonymous,
}

impl Caller {
    /// A bearer the middleware did not vouch for, for tests of what the tools
    /// do with one.
    #[cfg(test)]
    pub(crate) fn unverified(token: &str) -> Self {
        Self::Bearer(token.to_string())
    }
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Caller::Bearer(<redacted>)"),
            Self::Anonymous => f.write_str("Caller::Anonymous"),
        }
    }
}

/// What [`authenticate_caller`] needs: the deployment's authenticator, and
/// whether a request presenting no credential is admitted at all.
///
/// `anonymous` is decided once at startup: it is on when at least one declared
/// knowledge base grants `anyone` something and the operator has not set
/// `NOTEDTHAT_MCP_ANONYMOUS=never`. Access policies are a startup snapshot, so
/// this can be too. When it is off, a missing credential answers `401` with
/// the bearer challenge — which is what an OAuth-capable MCP client needs to
/// see before it will sign in.
#[derive(Debug)]
pub struct McpAuth {
    /// The deployment's one authenticator, shared with every other surface.
    pub authenticator: Arc<Authenticator>,
    /// Whether a request with no `Authorization` header is let through as
    /// [`Caller::Anonymous`].
    pub anonymous: bool,
}

/// JSON body returned with every 401 Unauthorized response.
#[derive(Debug, Serialize)]
struct UnauthorizedBody {
    error: &'static str,
    message: &'static str,
}

/// Axum middleware that establishes the [`Caller`] of an MCP HTTP request.
///
/// Reads the `Authorization` header from the incoming request and resolves it
/// through the [`Authenticator`]: the service token or a bearer token an
/// identity provider vouches for is the caller, and the request proceeds with
/// [`Caller::Bearer`] and the resolved [`notedthat_core::Principal`] in its
/// extensions.
///
/// A request with **no** `Authorization` header proceeds as
/// [`Caller::Anonymous`] when [`McpAuth::anonymous`] is set, and the tools
/// then act with no credential, bound by the manifests' `anyone` rules on the
/// API. Otherwise it is refused.
///
/// Returns **HTTP 401** with a JSON error body for any of:
/// - missing `Authorization` header, on a deployment that admits no anonymous
///   caller
/// - wrong authentication scheme (e.g. Basic, not Bearer)
/// - a token that is neither the service token nor verifiable
///
/// A supplied credential never downgrades to anonymous: a typo'd token is a
/// `401`, not a public view. The `401` carries a `WWW-Authenticate` bearer
/// challenge naming the protected-resource metadata when the deployment
/// publishes it, which is how an MCP client discovers where to obtain a token.
///
/// # Usage
///
/// ```rust,ignore
/// use axum::{Router, middleware};
/// use notedthat_mcp::auth::{McpAuth, authenticate_caller};
///
/// let auth = Arc::new(McpAuth {
///     authenticator: Arc::new(Authenticator::new("super-secret")),
///     anonymous: false,
/// });
/// let router = Router::new()
///     .route("/mcp", /* mcp service */)
///     .layer(middleware::from_fn_with_state(auth, authenticate_caller));
/// ```
pub async fn authenticate_caller(
    State(auth): State<Arc<McpAuth>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    match auth
        .authenticator
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
                .map(|token| Caller::Bearer(token.to_string()));
            if let Some(token) = token {
                request.extensions_mut().insert(token);
            }
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Ok(principal) if auth.anonymous => {
            request.extensions_mut().insert(Caller::Anonymous);
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
            if let Some(challenge) = auth.authenticator.bearer_challenge() {
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
    use notedthat_core::Principal;
    use tower::ServiceExt as _;

    /// A handler that reports the caller the middleware established, so a
    /// test can tell "admitted as anonymous" from "admitted as a bearer".
    async fn who(request: Request<Body>) -> &'static str {
        match request.extensions().get::<Caller>() {
            Some(Caller::Bearer(_)) => "bearer",
            Some(Caller::Anonymous) => {
                assert!(
                    request
                        .extensions()
                        .get::<Principal>()
                        .is_some_and(Principal::is_anonymous),
                    "an anonymous caller carries the anonymous principal"
                );
                "anonymous"
            }
            None => "nobody",
        }
    }

    fn app_with(authenticator: Authenticator, anonymous: bool) -> Router {
        Router::new()
            .route("/", get(who))
            .layer(middleware::from_fn_with_state(
                Arc::new(McpAuth {
                    authenticator: Arc::new(authenticator),
                    anonymous,
                }),
                authenticate_caller,
            ))
    }

    fn app(token: &str) -> Router {
        app_with(Authenticator::new(token), false)
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn missing_authorization_returns_401() {
        // Given: a request with no Authorization header, on a deployment that
        // admits no anonymous caller.
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();

        // When: the middleware processes the request.
        let res = app("secret").oneshot(req).await.unwrap();

        // Then: 401 Unauthorized is returned.
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_authorization_is_the_anonymous_caller_where_admitted() {
        // Given: a deployment that admits anonymous callers.
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();

        // When
        let res = app_with(Authenticator::new("secret"), true)
            .oneshot(req)
            .await
            .unwrap();

        // Then: the request reached the handler as the anonymous caller.
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body(res).await, "anonymous");
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
    async fn a_bad_credential_is_refused_even_where_anonymous_is_admitted() {
        // Given: a deployment that admits anonymous callers, and two requests
        // that supply a credential it cannot accept — a wrong bearer, and the
        // Basic scheme, which is WebDAV's and never MCP's.
        for authorization in ["Bearer sec-et", "Basic dXNlcjpwYXNz"] {
            let req = Request::builder()
                .uri("/")
                .header("Authorization", authorization)
                .body(Body::empty())
                .unwrap();

            // When
            let res = app_with(Authenticator::new("secret"), true)
                .oneshot(req)
                .await
                .unwrap();

            // Then: 401, never a quiet downgrade to the anonymous caller — a
            // typo'd token must not become a public view.
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "{authorization} must be refused"
            );
        }
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

        // Then: 200 OK — the request reached the inner handler as that bearer.
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body(res).await, "bearer");
    }

    #[tokio::test]
    async fn a_verified_identity_token_passes_through() {
        // Given: an authenticator with a verifier vouching for one token.
        let authenticator = Authenticator::new("secret").with_token_verifier(Arc::new(
            notedthat_core::testing::StubTokenVerifier::default().accepting(
                "jwt-alice",
                "alice",
                [],
            ),
        ));
        let req = Request::builder()
            .uri("/")
            .header("Authorization", "Bearer jwt-alice")
            .body(Body::empty())
            .unwrap();

        // When / Then
        let res = app_with(authenticator, false).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body(res).await, "bearer");
    }

    #[tokio::test]
    async fn a_401_keeps_its_json_body_and_adds_the_challenge_when_published() {
        // Given: a deployment publishing protected-resource metadata that
        // admits no anonymous caller — which is what `never` produces on a
        // deployment with public knowledge bases, and what a private
        // deployment produces regardless.
        let metadata_url = "https://notes.example.com/.well-known/oauth-protected-resource";
        let authenticator = Authenticator::new("secret").with_protected_resource(
            notedthat_core::ProtectedResource {
                resource: "https://notes.example.com".into(),
                authorization_servers: vec!["https://auth.example.com".into()],
                metadata_url: metadata_url.into(),
            },
        );

        // When
        let res = app_with(authenticator, false)
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        // Then: the challenge that sends an OAuth-capable client to the
        // authorization server survives.
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers().get("www-authenticate").unwrap(),
            &format!("Bearer resource_metadata=\"{metadata_url}\"")
        );
        let json: serde_json::Value = serde_json::from_str(&body(res).await).unwrap();
        assert_eq!(json["error"], "unauthorized");
    }
}
