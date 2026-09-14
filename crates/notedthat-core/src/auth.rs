//! Bearer token verification/extraction per RFC 6750 and Basic auth helpers per RFC 7617,
//! and the [`Authenticator`] every surface resolves its [`Principal`] through.

use crate::access::{Principal, UserIdentity};
use async_trait::async_trait;
use base64::Engine as _;
use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue};
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Verify a Bearer token in constant time.
///
/// # Token length leakage
///
/// This function short-circuits on length mismatch (returns `false` immediately
/// without invoking the constant-time comparison). This leaks the length of the
/// expected token, which is an **acceptable limitation** for a static Bearer token
/// per D21 (the token length is small and enumerable).
pub fn verify_bearer_token(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    if expected.is_empty() {
        return false;
    }
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Extract the Bearer token value from an `Authorization` header value.
///
/// The scheme match is **case-insensitive** per RFC 6750 §2.1. The token
/// is separated from the scheme by exactly one space; tabs and multiple
/// consecutive spaces are rejected.
///
/// Returns `None` if the header value is malformed or missing a token.
pub fn extract_bearer_from_header(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    if rest.is_empty() || rest.starts_with(' ') {
        return None;
    }
    Some(rest)
}

/// Verify HTTP Basic credentials in constant time.
///
/// Empty usernames/passwords are rejected. Length mismatches are rejected before
/// comparison, matching the accepted Bearer-token length leakage precedent.
pub fn verify_basic_credentials(
    provided_username: &str,
    provided_password: &str,
    expected_username: &str,
    expected_password: &str,
) -> bool {
    if provided_username.is_empty()
        || provided_password.is_empty()
        || expected_username.is_empty()
        || expected_password.is_empty()
    {
        return false;
    }

    if provided_username.len() != expected_username.len()
        || provided_password.len() != expected_password.len()
    {
        return false;
    }

    let username_match = provided_username
        .as_bytes()
        .ct_eq(expected_username.as_bytes());
    let password_match = provided_password
        .as_bytes()
        .ct_eq(expected_password.as_bytes());
    let both_match = username_match & password_match;
    bool::from(both_match)
}

/// Extract a non-empty username and password from an HTTP Basic `Authorization` header.
///
/// The scheme match is case-insensitive. Credentials are decoded with the
/// standard Base64 alphabet and split on the first colon so passwords may
/// contain colons.
pub fn extract_basic_from_header(value: &str) -> Option<(String, String)> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    if rest.starts_with(' ') {
        return None;
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest)
        .ok()?;
    let decoded_str = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded_str.split_once(':')?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username.to_string(), password.to_string()))
}

/// A credential was supplied and did not verify.
///
/// Distinct from "no credential", which is [`Principal::Anyone`] and may still
/// be granted access — the whole point of the distinction is that a supplied
/// credential never silently downgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialRefused;

/// Which `Authorization` schemes a surface accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schemes {
    /// `Bearer` only — the HTTP API, the browse pages and MCP.
    Bearer,
    /// `Basic` as well — `WebDAV`, whose clients prompt for a username and password.
    BasicOrBearer,
}

/// Why a bearer token an identity provider was asked about did not verify.
///
/// Carries a reason for the log line and nothing else: never the token, never
/// its claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRejected {
    /// A short operator-facing reason, safe to log.
    pub reason: String,
}

impl TokenRejected {
    /// Build a rejection from its reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Something that can turn a bearer token into the identity behind it.
///
/// The one seam between the surfaces and an identity provider: the server
/// implements it over OIDC discovery and a JWKS, and tests implement it over a
/// map. Consulted only after the service token failed to match.
#[async_trait]
pub trait TokenVerifier: Send + Sync {
    /// Verify `token` and return the identity it vouches for.
    async fn verify(&self, token: &str) -> Result<UserIdentity, TokenRejected>;
}

/// The RFC 9728 protected-resource metadata this deployment publishes.
///
/// Served at `/.well-known/oauth-protected-resource` and named in the
/// `WWW-Authenticate` challenge on `401`s, which is how an MCP client finds
/// the authorization server to obtain a token from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResource {
    /// The resource identifier: this deployment's public URL.
    pub resource: String,
    /// The issuers that mint tokens for it.
    pub authorization_servers: Vec<String>,
    /// Where the metadata document itself is served.
    pub metadata_url: String,
}

impl ProtectedResource {
    /// The metadata document, as JSON.
    pub fn document(&self) -> serde_json::Value {
        serde_json::json!({
            "resource": self.resource,
            "authorization_servers": self.authorization_servers,
            "bearer_methods_supported": ["header"],
        })
    }
}

/// The single definition of the credential rules, shared by every surface.
///
/// Absent header → [`Principal::Anyone`]. Exactly one `Authorization` header
/// carrying the service token, the `WebDAV` Basic pair (where accepted), or a
/// bearer token the [`TokenVerifier`] vouches for → [`Principal::SignedIn`].
/// Anything else — a second header, an unknown scheme, a token nobody
/// recognises — is [`CredentialRefused`], never a downgrade to anonymous.
pub struct Authenticator {
    service_token: String,
    basic: Option<(String, String)>,
    verifier: Option<Arc<dyn TokenVerifier>>,
    protected_resource: Option<ProtectedResource>,
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("service_token", &"<redacted>")
            .field(
                "basic",
                &self.basic.as_ref().map(|(user, _)| (user, "<redacted>")),
            )
            .field("verifier", &self.verifier.as_ref().map(|_| "<configured>"))
            .field("protected_resource", &self.protected_resource)
            .finish()
    }
}

impl Authenticator {
    /// An authenticator that recognises only `service_token` as a bearer.
    pub fn new(service_token: impl Into<String>) -> Self {
        Self {
            service_token: service_token.into(),
            basic: None,
            verifier: None,
            protected_resource: None,
        }
    }

    /// Also accept this `Basic` username and password, on surfaces that allow it.
    #[must_use]
    pub fn with_basic(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.basic = Some((username.into(), password.into()));
        self
    }

    /// Also accept bearer tokens `verifier` vouches for.
    #[must_use]
    pub fn with_token_verifier(mut self, verifier: Arc<dyn TokenVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Publish RFC 9728 metadata and name it in bearer challenges.
    #[must_use]
    pub fn with_protected_resource(mut self, resource: ProtectedResource) -> Self {
        self.protected_resource = Some(resource);
        self
    }

    /// Whether bearer tokens other than the service token can verify at all.
    pub fn accepts_identity_tokens(&self) -> bool {
        self.verifier.is_some()
    }

    /// The published protected-resource metadata, if configured.
    pub fn protected_resource(&self) -> Option<&ProtectedResource> {
        self.protected_resource.as_ref()
    }

    /// The `WWW-Authenticate` value a bearer-only surface adds to a `401`.
    ///
    /// Only present when metadata is published: a bare `Bearer` challenge tells
    /// a client nothing it did not know, and browsers ignore it, so there is
    /// nothing to gain from emitting one.
    pub fn bearer_challenge(&self) -> Option<HeaderValue> {
        let resource = self.protected_resource.as_ref()?;
        HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}\"",
            resource.metadata_url
        ))
        .ok()
    }

    /// Resolve the principal behind a request's `Authorization` headers.
    ///
    /// # Errors
    ///
    /// [`CredentialRefused`] when an `Authorization` header is present and does
    /// not carry exactly one credential this authenticator accepts.
    pub async fn resolve(
        &self,
        headers: &HeaderMap,
        accept: Schemes,
    ) -> Result<Principal, CredentialRefused> {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let Some(header) = values.next() else {
            return Ok(Principal::Anyone);
        };
        if values.next().is_some() {
            return Err(CredentialRefused);
        }
        let header = header.to_str().map_err(|_| CredentialRefused)?;

        if let Some(token) = extract_bearer_from_header(header) {
            return self.resolve_bearer(token).await;
        }
        if accept == Schemes::BasicOrBearer
            && let Some((username, password)) = extract_basic_from_header(header)
            && let Some((expected_user, expected_pass)) = &self.basic
            && verify_basic_credentials(&username, &password, expected_user, expected_pass)
        {
            return Ok(Principal::service_token());
        }
        Err(CredentialRefused)
    }

    async fn resolve_bearer(&self, token: &str) -> Result<Principal, CredentialRefused> {
        // The service token is checked first and in constant time; the verifier
        // is only consulted for something that is not it, so a deployment
        // without an identity provider never changes behaviour.
        if verify_bearer_token(token, &self.service_token) {
            return Ok(Principal::service_token());
        }
        let Some(verifier) = &self.verifier else {
            return Err(CredentialRefused);
        };
        match verifier.verify(token).await {
            Ok(identity) => Ok(Principal::SignedIn(crate::access::Identity::User(identity))),
            Err(rejected) => {
                tracing::debug!(reason = %rejected.reason, "bearer token rejected");
                Err(CredentialRefused)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_bearer_token_matching() {
        assert!(verify_bearer_token("secret-token", "secret-token"));
    }

    #[test]
    fn test_verify_bearer_token_different_same_length() {
        assert!(!verify_bearer_token("aaaabbbb", "aaaacccc"));
    }

    #[test]
    fn test_verify_bearer_token_empty_provided() {
        assert!(!verify_bearer_token("", "some-token"));
    }

    #[test]
    fn test_verify_bearer_token_empty_expected() {
        assert!(!verify_bearer_token("some-token", ""));
    }

    #[test]
    fn test_verify_bearer_token_different_lengths() {
        assert!(!verify_bearer_token("short", "much-longer-token"));
    }

    #[test]
    fn test_verify_bearer_token_case_sensitive() {
        assert!(!verify_bearer_token("AbC", "abc"));
    }

    #[test]
    fn test_verify_bearer_token_whitespace_sensitive() {
        assert!(!verify_bearer_token("tok", "tok "));
    }

    #[test]
    fn test_extract_bearer_lowercase_scheme() {
        assert_eq!(extract_bearer_from_header("Bearer abc123"), Some("abc123"));
    }

    #[test]
    fn test_extract_bearer_lowercase_bearer() {
        assert_eq!(extract_bearer_from_header("bearer abc123"), Some("abc123"));
    }

    #[test]
    fn test_extract_bearer_uppercase_scheme() {
        assert_eq!(extract_bearer_from_header("BEARER abc123"), Some("abc123"));
    }

    #[test]
    fn test_extract_bearer_wrong_scheme_basic() {
        assert_eq!(extract_bearer_from_header("Basic dXNlcjpwYXNz"), None);
    }

    #[test]
    fn test_extract_bearer_no_scheme() {
        assert_eq!(extract_bearer_from_header("abc123"), None);
    }

    #[test]
    fn test_extract_bearer_empty_header() {
        assert_eq!(extract_bearer_from_header(""), None);
    }

    #[test]
    fn test_extract_bearer_empty_token_after_space() {
        assert_eq!(extract_bearer_from_header("Bearer "), None);
    }

    #[test]
    fn test_extract_bearer_double_space_rejected() {
        assert_eq!(extract_bearer_from_header("Bearer  abc"), None);
    }

    #[test]
    fn test_extract_bearer_tab_not_space_rejected() {
        assert_eq!(extract_bearer_from_header("Bearer\tabc"), None);
    }

    #[test]
    fn test_extract_bearer_mixed_case_scheme() {
        assert_eq!(
            extract_bearer_from_header("BeArEr mytoken"),
            Some("mytoken")
        );
    }

    #[test]
    fn test_extract_bearer_token_with_dots() {
        assert_eq!(
            extract_bearer_from_header("Bearer eyJ0.eyJz.SflK"),
            Some("eyJ0.eyJz.SflK")
        );
    }

    #[test]
    fn test_verify_bearer_both_empty() {
        assert!(!verify_bearer_token("", ""));
    }

    #[test]
    fn verify_matching_credentials() {
        assert!(verify_basic_credentials("user", "pass", "user", "pass"));
    }

    #[test]
    fn verify_wrong_username() {
        assert!(!verify_basic_credentials("user", "pass", "xxxx", "pass"));
    }

    #[test]
    fn verify_wrong_password() {
        assert!(!verify_basic_credentials("user", "pass", "user", "xxxx"));
    }

    #[test]
    fn verify_empty_provided_username() {
        assert!(!verify_basic_credentials("", "pass", "user", "pass"));
    }

    #[test]
    fn verify_empty_provided_password() {
        assert!(!verify_basic_credentials("user", "", "user", "pass"));
    }

    #[test]
    fn verify_empty_expected_username() {
        assert!(!verify_basic_credentials("user", "pass", "", "pass"));
    }

    #[test]
    fn verify_empty_expected_password() {
        assert!(!verify_basic_credentials("user", "pass", "user", ""));
    }

    #[test]
    fn verify_different_lengths_username_longer() {
        assert!(!verify_basic_credentials("users", "pass", "user", "pass"));
    }

    #[test]
    fn verify_different_lengths_username_shorter() {
        assert!(!verify_basic_credentials("usr", "pass", "user", "pass"));
    }

    #[test]
    fn verify_different_lengths_password_longer() {
        assert!(!verify_basic_credentials("user", "passw", "user", "pass"));
    }

    #[test]
    fn verify_different_lengths_password_shorter() {
        assert!(!verify_basic_credentials("user", "pas", "user", "pass"));
    }

    #[test]
    fn extract_basic_valid() {
        assert_eq!(
            extract_basic_from_header("Basic dXNlcjpwYXNz"),
            Some(("user".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn extract_basic_mixed_case_scheme() {
        assert_eq!(
            extract_basic_from_header("bAsIc dXNlcjpwYXNz"),
            Some(("user".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn extract_basic_wrong_scheme() {
        assert_eq!(extract_basic_from_header("Bearer dXNlcjpwYXNz"), None);
    }

    #[test]
    fn extract_basic_empty_header() {
        assert_eq!(extract_basic_from_header(""), None);
    }

    #[test]
    fn extract_basic_double_space_rejected() {
        assert_eq!(extract_basic_from_header("Basic  dXNlcjpwYXNz"), None);
    }

    #[test]
    fn extract_basic_invalid_base64() {
        assert_eq!(extract_basic_from_header("Basic not-valid-base64!!!"), None);
    }

    #[test]
    fn extract_basic_non_utf8_after_decode() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe]);
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_no_colon() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("userpass");
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_colon_at_start() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(":pass");
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_colon_at_end() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:");
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_password_with_colon() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass:word");
        assert_eq!(
            extract_basic_from_header(&format!("Basic {encoded}")),
            Some(("user".to_string(), "pass:word".to_string()))
        );
    }
}

#[cfg(test)]
mod authenticator_tests {
    use super::*;
    use crate::access::Identity;
    use crate::testing::StubTokenVerifier;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(AUTHORIZATION, HeaderValue::from_str(value).expect("header"));
        }
        headers
    }

    fn with_stub() -> Authenticator {
        Authenticator::new("service")
            .with_basic("dav-user", "dav-pass")
            .with_token_verifier(Arc::new(StubTokenVerifier::default().accepting(
                "jwt-alice",
                "alice",
                ["editors"],
            )))
    }

    #[tokio::test]
    async fn an_absent_header_is_anonymous() {
        assert_eq!(
            with_stub()
                .resolve(&HeaderMap::new(), Schemes::Bearer)
                .await,
            Ok(Principal::Anyone)
        );
    }

    #[tokio::test]
    async fn the_service_token_wins_before_the_verifier_is_consulted() {
        // Given — a verifier that would also accept "service" as a user.
        let auth = Authenticator::new("service").with_token_verifier(Arc::new(
            StubTokenVerifier::default().accepting("service", "impostor", []),
        ));

        // When / Then
        assert_eq!(
            auth.resolve(&headers(&["Bearer service"]), Schemes::Bearer)
                .await,
            Ok(Principal::service_token())
        );
    }

    #[tokio::test]
    async fn a_verified_identity_token_is_signed_in_as_that_user() {
        // Given / When
        let principal = with_stub()
            .resolve(&headers(&["Bearer jwt-alice"]), Schemes::Bearer)
            .await
            .expect("verified");

        // Then
        let Principal::SignedIn(Identity::User(user)) = principal else {
            panic!("expected a user identity, got {principal:?}");
        };
        assert_eq!(user.subject, "alice");
        assert!(user.groups.contains("editors"));
    }

    #[tokio::test]
    async fn a_bearer_that_is_neither_the_service_token_nor_verifiable_is_refused_not_downgraded() {
        assert_eq!(
            with_stub()
                .resolve(&headers(&["Bearer nobody"]), Schemes::Bearer)
                .await,
            Err(CredentialRefused)
        );
    }

    #[tokio::test]
    async fn without_a_verifier_every_foreign_bearer_is_refused() {
        assert_eq!(
            Authenticator::new("service")
                .resolve(&headers(&["Bearer jwt-alice"]), Schemes::Bearer)
                .await,
            Err(CredentialRefused)
        );
    }

    #[tokio::test]
    async fn two_authorization_headers_are_refused() {
        assert_eq!(
            with_stub()
                .resolve(
                    &headers(&["Bearer service", "Bearer service"]),
                    Schemes::Bearer
                )
                .await,
            Err(CredentialRefused)
        );
    }

    #[tokio::test]
    async fn a_basic_credential_is_refused_where_only_bearer_is_accepted() {
        // Given — the WebDAV pair, presented to the API.
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("dav-user:dav-pass")
        );

        // When / Then
        assert_eq!(
            with_stub()
                .resolve(&headers(&[&basic]), Schemes::Bearer)
                .await,
            Err(CredentialRefused)
        );
        assert_eq!(
            with_stub()
                .resolve(&headers(&[&basic]), Schemes::BasicOrBearer)
                .await,
            Ok(Principal::service_token())
        );
    }

    #[tokio::test]
    async fn a_wrong_basic_credential_is_refused_and_basic_needs_a_configured_pair() {
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("dav-user:wrong")
        );
        assert_eq!(
            with_stub()
                .resolve(&headers(&[&basic]), Schemes::BasicOrBearer)
                .await,
            Err(CredentialRefused)
        );
        assert_eq!(
            Authenticator::new("service")
                .resolve(&headers(&[&basic]), Schemes::BasicOrBearer)
                .await,
            Err(CredentialRefused)
        );
    }

    #[tokio::test]
    async fn a_bearer_is_accepted_on_a_basic_surface_too() {
        assert_eq!(
            with_stub()
                .resolve(&headers(&["Bearer jwt-alice"]), Schemes::BasicOrBearer)
                .await
                .map(|p| p.is_signed_in()),
            Ok(true)
        );
    }

    #[test]
    fn debug_output_never_contains_a_secret() {
        let auth = Authenticator::new("sk-top-secret").with_basic("dav-user", "dav-pass");
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("sk-top-secret"), "{rendered}");
        assert!(!rendered.contains("dav-pass"), "{rendered}");
        assert!(rendered.contains("dav-user"), "{rendered}");
    }

    #[test]
    fn the_challenge_names_the_metadata_document_only_when_published() {
        // Given
        let bare = Authenticator::new("service");
        let published = Authenticator::new("service").with_protected_resource(ProtectedResource {
            resource: "https://notes.example.com".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            metadata_url: "https://notes.example.com/.well-known/oauth-protected-resource".into(),
        });

        // When / Then
        assert!(bare.bearer_challenge().is_none());
        assert_eq!(
            published.bearer_challenge().expect("challenge"),
            "Bearer resource_metadata=\"https://notes.example.com/.well-known/oauth-protected-resource\""
        );
        assert_eq!(
            published
                .protected_resource()
                .expect("published")
                .document(),
            serde_json::json!({
                "resource": "https://notes.example.com",
                "authorization_servers": ["https://auth.example.com"],
                "bearer_methods_supported": ["header"],
            })
        );
    }
}
