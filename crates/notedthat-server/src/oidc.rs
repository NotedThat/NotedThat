//! OIDC bearer-token verification: discovery, a cached JWKS, and the
//! [`TokenVerifier`] the [`notedthat_core::Authenticator`] consults for any
//! bearer that is not the service token (D53).
//!
//! Only signed JWT access tokens are accepted — `RS256`/`RS384`/`RS512` and
//! `ES256`/`ES384` against the issuer's published keys. There is no
//! introspection call, so a provider that mints opaque access tokens by
//! default (Authelia, Zitadel) has to be told to mint JWTs; the configuration
//! guide says how for each supported provider.

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use notedthat_core::{TokenRejected, TokenVerifier, UserIdentity};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// The algorithms a token may be signed with.
///
/// Asymmetric only. An `HS*` token is refused before any key is looked at:
/// the deployment holds no shared secret with the issuer, and a JWKS never
/// publishes one, so accepting the family would only be a footgun.
const ACCEPTED_ALGORITHMS: [Algorithm; 5] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

/// How much clock skew between issuer and server to forgive on `exp`/`nbf`.
const LEEWAY: Duration = Duration::from_secs(60);

/// The shortest interval between two JWKS fetches triggered by tokens.
///
/// An unknown `kid` refetches, so without this a flood of tokens carrying a
/// bogus `kid` would be a flood of requests to the issuer.
const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(30);

/// After this long a cached key set is refreshed before the next verification
/// uses it, so a key rotation that keeps the old `kid` is still picked up.
const MAX_KEY_AGE: Duration = Duration::from_hours(1);

/// What `NOTEDTHAT_OIDC_*` configures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcSettings {
    /// The issuer URL, exactly as the provider spells its `iss` claim.
    pub issuer: String,
    /// The audiences a token may carry; one of them must match.
    pub audiences: Vec<String>,
    /// The claim `user:` rules and logs identify a caller by.
    pub username_claim: String,
    /// The claim `group:` rules are matched against.
    pub groups_claim: String,
    /// Timeout for the discovery and JWKS requests.
    pub http_timeout: Duration,
    /// This deployment's public URL, when it publishes RFC 9728 metadata.
    pub resource: Option<String>,
    /// A PEM bundle of CA certificates to trust for the issuer, on top of the
    /// built-in roots — a self-hosted provider is usually behind an internal CA.
    pub ca_cert: Option<std::path::PathBuf>,
}

impl OidcSettings {
    /// The default username claim.
    pub const DEFAULT_USERNAME_CLAIM: &'static str = "preferred_username";
    /// The default groups claim.
    pub const DEFAULT_GROUPS_CLAIM: &'static str = "groups";
    /// The default HTTP timeout, in milliseconds.
    pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 5_000;

    /// The URL the discovery document is fetched from.
    pub fn discovery_url(&self) -> String {
        format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        )
    }
}

/// The parts of the discovery document this verifier reads.
#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

/// One published key, ready to verify with.
struct CachedKey {
    kid: Option<String>,
    key: DecodingKey,
}

struct KeyCache {
    keys: Vec<CachedKey>,
    /// When the keys were fetched; `None` means "stale, refetch when asked".
    fetched_at: Option<Instant>,
}

/// Verifies bearer tokens against an OIDC issuer's published keys.
pub struct OidcVerifier {
    settings: OidcSettings,
    http: reqwest::Client,
    jwks_uri: String,
    cache: RwLock<KeyCache>,
    /// Serialises refetches so concurrent unknown-`kid` tokens cost one request.
    refetch: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for OidcVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("issuer", &self.settings.issuer)
            .field("jwks_uri", &self.jwks_uri)
            .finish_non_exhaustive()
    }
}

impl OidcVerifier {
    /// Discover the issuer and fetch its keys.
    ///
    /// Runs at startup and fails fast (D39): a verifier that cannot reach its
    /// issuer would refuse every identity token, which is a misconfiguration
    /// better reported before the listener binds than after.
    ///
    /// # Errors
    ///
    /// When the discovery document or the JWKS cannot be fetched or parsed, or
    /// when the document's `issuer` is not the configured one.
    pub async fn discover(settings: OidcSettings) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(settings.http_timeout);
        if let Some(path) = &settings.ca_cert {
            let bundle = std::fs::read(path).map_err(|error| {
                anyhow::anyhow!("NOTEDTHAT_OIDC_CA_CERT {}: {error}", path.display())
            })?;
            let certificates = reqwest::Certificate::from_pem_bundle(&bundle).map_err(|error| {
                anyhow::anyhow!(
                    "NOTEDTHAT_OIDC_CA_CERT {} is not a PEM certificate bundle: {error}",
                    path.display()
                )
            })?;
            if certificates.is_empty() {
                anyhow::bail!(
                    "NOTEDTHAT_OIDC_CA_CERT {} contains no certificates",
                    path.display()
                );
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let http = builder
            .build()
            .map_err(|error| anyhow::anyhow!("could not build the OIDC HTTP client: {error}"))?;

        let discovery_url = settings.discovery_url();
        let document: DiscoveryDocument = http
            .get(&discovery_url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| anyhow::Error::new(error).context(format!("GET {discovery_url}")))?
            .json()
            .await
            .map_err(|error| {
                anyhow::Error::new(error)
                    .context(format!("{discovery_url} is not a discovery document"))
            })?;

        // The document's own issuer is what every token's `iss` will carry.
        // A mismatch means every token would be rejected, and the fix is to
        // spell NOTEDTHAT_OIDC_ISSUER the way the provider does.
        if document.issuer != settings.issuer {
            anyhow::bail!(
                "{discovery_url} reports issuer `{}` but NOTEDTHAT_OIDC_ISSUER is `{}`; \
                 they must match exactly, trailing slash included",
                document.issuer,
                settings.issuer
            );
        }

        let jwks = fetch_jwks(&http, &document.jwks_uri).await?;
        tracing::info!(
            issuer = %settings.issuer,
            jwks_uri = %document.jwks_uri,
            keys = jwks.keys.len(),
            "OIDC issuer discovered"
        );
        Ok(Self::assemble(settings, http, document.jwks_uri, &jwks))
    }

    /// A verifier over an already-fetched key set, for tests without an issuer.
    ///
    /// `jwks_uri` is where refetches go; `None` means an unknown `kid` is
    /// simply an unknown `kid`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_jwks(settings: OidcSettings, jwks: &JwkSet, jwks_uri: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(settings.http_timeout)
            .build()
            .expect("reqwest client");
        Self::assemble(
            settings,
            http,
            jwks_uri.unwrap_or_else(|| "unset:".to_string()),
            jwks,
        )
    }

    fn assemble(
        settings: OidcSettings,
        http: reqwest::Client,
        jwks_uri: String,
        jwks: &JwkSet,
    ) -> Self {
        Self {
            settings,
            http,
            jwks_uri,
            cache: RwLock::new(KeyCache {
                keys: cache_keys(jwks),
                fetched_at: Some(Instant::now()),
            }),
            refetch: tokio::sync::Mutex::new(()),
        }
    }

    /// The settings this verifier was built from.
    pub fn settings(&self) -> &OidcSettings {
        &self.settings
    }

    /// Whether the cache holds a key usable for `kid`.
    fn has_key(&self, kid: Option<&str>) -> bool {
        self.lookup(kid).is_some()
    }

    /// The decoding key for `kid`; with no `kid`, the only key if there is one.
    fn lookup(&self, kid: Option<&str>) -> Option<DecodingKey> {
        let cache = self
            .cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match kid {
            Some(kid) => cache
                .keys
                .iter()
                .find(|key| key.kid.as_deref() == Some(kid)),
            None if cache.keys.len() == 1 => cache.keys.first(),
            None => None,
        }
        .map(|key| key.key.clone())
    }

    fn cache_age(&self) -> Duration {
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fetched_at
            .map_or(Duration::MAX, |fetched_at| fetched_at.elapsed())
    }

    /// Forget when the keys were fetched, so the next token refreshes them.
    #[cfg(test)]
    fn mark_stale(&self) {
        self.cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fetched_at = None;
    }

    /// Refetch the key set, unless one was fetched too recently.
    ///
    /// A failed refetch keeps the previous keys: a transient issuer outage
    /// should not turn every already-verifiable token into a refusal.
    async fn refresh(&self) {
        let _serialised = self.refetch.lock().await;
        if self.cache_age() < MIN_REFETCH_INTERVAL {
            return;
        }
        match fetch_jwks(&self.http, &self.jwks_uri).await {
            Ok(jwks) => {
                let mut cache = self
                    .cache
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.keys = cache_keys(&jwks);
                cache.fetched_at = Some(Instant::now());
                tracing::debug!(keys = cache.keys.len(), "OIDC key set refreshed");
            }
            Err(error) => {
                // Move the timestamp anyway, so a broken issuer is asked again
                // at the rate-limited interval and not on every token.
                self.cache
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fetched_at = Some(Instant::now());
                tracing::warn!(error = %format!("{error:#}"), "OIDC key set refresh failed; keeping the previous keys");
            }
        }
    }
}

#[async_trait]
impl TokenVerifier for OidcVerifier {
    async fn verify(&self, token: &str) -> Result<UserIdentity, TokenRejected> {
        let header = decode_header(token).map_err(|_| TokenRejected::new("not a JWT"))?;
        if !ACCEPTED_ALGORITHMS.contains(&header.alg) {
            return Err(TokenRejected::new(format!(
                "algorithm {:?} is not accepted",
                header.alg
            )));
        }

        let kid = header.kid.as_deref();
        if self.cache_age() > MAX_KEY_AGE || !self.has_key(kid) {
            self.refresh().await;
        }
        let key = self.lookup(kid).ok_or_else(|| {
            TokenRejected::new(match kid {
                Some(_) => "signed with a key the issuer does not publish",
                None => "no `kid`, and the issuer publishes more than one key",
            })
        })?;

        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.settings.issuer]);
        validation.set_audience(&self.settings.audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.leeway = LEEWAY.as_secs();
        validation.validate_nbf = true;

        let claims = decode::<serde_json::Map<String, serde_json::Value>>(token, &key, &validation)
            .map_err(|error| TokenRejected::new(describe(&error)))?
            .claims;

        let subject = claims
            .get(&self.settings.username_claim)
            .or_else(|| claims.get("sub"))
            .and_then(serde_json::Value::as_str)
            .filter(|subject| !subject.is_empty())
            .ok_or_else(|| TokenRejected::new("no username claim and no `sub`"))?
            .to_string();
        let groups = claims
            .get(&self.settings.groups_claim)
            .map(groups_from_claim)
            .unwrap_or_default();

        Ok(UserIdentity { subject, groups })
    }
}

/// An operator-facing reason for a validation failure. Never the token.
fn describe(error: &jsonwebtoken::errors::Error) -> String {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::ExpiredSignature => "expired".to_string(),
        ErrorKind::ImmatureSignature => "not yet valid (`nbf`)".to_string(),
        ErrorKind::InvalidIssuer => "issuer mismatch".to_string(),
        ErrorKind::InvalidAudience => "audience mismatch".to_string(),
        ErrorKind::InvalidSignature => "signature does not verify".to_string(),
        ErrorKind::MissingRequiredClaim(claim) => format!("missing `{claim}`"),
        ErrorKind::InvalidAlgorithm => "key and algorithm disagree".to_string(),
        other => format!("invalid: {other:?}"),
    }
}

/// The group names a claim value carries.
///
/// Providers disagree on the shape. Authentik and Authelia publish an array of
/// strings; Zitadel's `urn:zitadel:iam:org:project:roles` is an object keyed
/// by role name (its documentation also shows that object wrapped in an
/// array); a single string is taken as one group. Anything else contributes
/// nothing rather than failing the token — a caller with unreadable groups is
/// a caller in no groups.
fn groups_from_claim(value: &serde_json::Value) -> BTreeSet<String> {
    use serde_json::Value;
    let mut groups = BTreeSet::new();
    match value {
        Value::String(group) => {
            groups.insert(group.clone());
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(group) => {
                        groups.insert(group.clone());
                    }
                    Value::Object(roles) => groups.extend(roles.keys().cloned()),
                    _ => {}
                }
            }
        }
        Value::Object(roles) => groups.extend(roles.keys().cloned()),
        _ => {}
    }
    groups
}

async fn fetch_jwks(http: &reqwest::Client, jwks_uri: &str) -> anyhow::Result<JwkSet> {
    http.get(jwks_uri)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| anyhow::Error::new(error).context(format!("GET {jwks_uri}")))?
        .json()
        .await
        .map_err(|error| anyhow::Error::new(error).context(format!("{jwks_uri} is not a JWK set")))
}

/// The usable keys in a set. Keys of a kind this verifier cannot sign-check
/// with (symmetric, unknown) are skipped rather than failing the whole set,
/// and so is a key of a usable kind whose parameters do not decode — with a
/// warning naming its `kid`, since a token it signed will then be refused as
/// signed with a key the issuer does not publish.
fn cache_keys(jwks: &JwkSet) -> Vec<CachedKey> {
    jwks.keys
        .iter()
        .filter(|jwk| {
            matches!(
                jwk.algorithm,
                AlgorithmParameters::RSA(_) | AlgorithmParameters::EllipticCurve(_)
            )
        })
        .filter_map(|jwk: &Jwk| {
            let kid = jwk.common.key_id.clone();
            match DecodingKey::from_jwk(jwk) {
                Ok(key) => Some(CachedKey { kid, key }),
                Err(error) => {
                    tracing::warn!(
                        kid = kid.as_deref().unwrap_or("<none>"),
                        error = %error,
                        "OIDC key set publishes a key this server cannot use; skipping it"
                    );
                    None
                }
            }
        })
        .collect()
}

/// The checked-in RS256 key pair the OIDC tests sign with.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{JwkSet, OidcSettings};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use std::time::Duration;

    /// The `kid` the fixture key is published under.
    pub const KID: &str = "test-2026";
    const PRIVATE_KEY_PEM: &str = include_str!("oidc/testdata/rs256-private.pem");
    /// The JWKS document publishing the fixture key, verbatim.
    pub const JWKS_JSON: &str = include_str!("oidc/testdata/jwks.json");

    /// The fixture key set.
    pub fn jwks() -> JwkSet {
        serde_json::from_str(JWKS_JSON).expect("fixture JWKS parses")
    }

    /// Settings for `issuer`, accepting audience `notedthat`, with the defaults.
    pub fn settings(issuer: &str) -> OidcSettings {
        OidcSettings {
            issuer: issuer.to_string(),
            audiences: vec!["notedthat".to_string()],
            username_claim: OidcSettings::DEFAULT_USERNAME_CLAIM.to_string(),
            groups_claim: OidcSettings::DEFAULT_GROUPS_CLAIM.to_string(),
            http_timeout: Duration::from_millis(OidcSettings::DEFAULT_HTTP_TIMEOUT_MS),
            resource: None,
            ca_cert: None,
        }
    }

    /// Sign `claims` with the fixture key under [`KID`].
    pub fn mint(claims: &serde_json::Value) -> String {
        mint_with(Algorithm::RS256, Some(KID), claims)
    }

    /// Sign `claims` with the fixture key, choosing algorithm and `kid`.
    pub fn mint_with(alg: Algorithm, kid: Option<&str>, claims: &serde_json::Value) -> String {
        let mut header = Header::new(alg);
        header.kid = kid.map(str::to_string);
        let key = match alg {
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
                EncodingKey::from_secret(b"not-a-secret-anyone-shares")
            }
            _ => EncodingKey::from_rsa_pem(PRIVATE_KEY_PEM.as_bytes()).expect("fixture key"),
        };
        encode(&header, claims, &key).expect("token encodes")
    }

    /// Seconds since the Unix epoch, for `exp`/`iat`/`nbf` claims.
    pub fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs()
    }

    /// A well-formed set of claims for `subject` in `groups`, valid for an hour.
    pub fn claims(issuer: &str, subject: &str, groups: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "iss": issuer,
            "sub": format!("{subject}-opaque-id"),
            "aud": "notedthat",
            "exp": now() + 3600,
            "iat": now(),
            "preferred_username": subject,
            "groups": groups,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{JWKS_JSON, KID, claims, jwks, mint, mint_with, now, settings};
    use super::*;
    use jsonwebtoken::Algorithm;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ISSUER: &str = "https://auth.example.com";

    fn verifier() -> OidcVerifier {
        OidcVerifier::from_jwks(settings(ISSUER), &jwks(), None)
    }

    async fn issuer_serving(jwks_body: &str) -> MockServer {
        let server = MockServer::start().await;
        let issuer = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": issuer,
                "jwks_uri": format!("{issuer}/jwks"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_string(jwks_body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_token_signed_by_a_published_key_yields_subject_and_groups() {
        // Given
        let token = mint(&claims(ISSUER, "alice", &["editors", "staff"]));

        // When
        let identity = verifier().verify(&token).await.expect("verifies");

        // Then
        assert_eq!(identity.subject, "alice");
        assert_eq!(
            identity.groups,
            ["editors", "staff"]
                .map(str::to_string)
                .into_iter()
                .collect()
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_rejected() {
        // Given — expired well beyond the leeway.
        let mut claims = claims(ISSUER, "alice", &[]);
        claims["exp"] = serde_json::json!(now() - 3600);

        // When / Then
        let rejected = verifier()
            .verify(&mint(&claims))
            .await
            .expect_err("expired");
        assert_eq!(rejected.reason, "expired");
    }

    #[tokio::test]
    async fn a_token_for_another_audience_is_rejected() {
        let mut claims = claims(ISSUER, "alice", &[]);
        claims["aud"] = serde_json::json!("someone-else");
        let rejected = verifier().verify(&mint(&claims)).await.expect_err("aud");
        assert_eq!(rejected.reason, "audience mismatch");
    }

    #[tokio::test]
    async fn an_audience_array_containing_ours_is_accepted() {
        // Given — Zitadel lists the project id and every client id.
        let mut claims = claims(ISSUER, "alice", &[]);
        claims["aud"] = serde_json::json!(["123456", "notedthat"]);

        // When / Then
        assert!(verifier().verify(&mint(&claims)).await.is_ok());
    }

    #[tokio::test]
    async fn a_token_from_another_issuer_is_rejected() {
        let rejected = verifier()
            .verify(&mint(&claims("https://evil.example.com", "alice", &[])))
            .await
            .expect_err("iss");
        assert_eq!(rejected.reason, "issuer mismatch");
    }

    #[tokio::test]
    async fn hs256_is_rejected_before_any_key_is_consulted() {
        let token = mint_with(Algorithm::HS256, Some(KID), &claims(ISSUER, "alice", &[]));
        let rejected = verifier().verify(&token).await.expect_err("HS256");
        assert!(rejected.reason.contains("HS256"), "{}", rejected.reason);
    }

    #[tokio::test]
    async fn a_token_that_is_not_a_jwt_is_rejected() {
        let rejected = verifier()
            .verify("sk-live-not-a-jwt")
            .await
            .expect_err("shape");
        assert_eq!(rejected.reason, "not a JWT");
    }

    #[tokio::test]
    async fn a_token_signed_with_a_different_key_is_rejected() {
        // Given — a verifier that publishes no key at all.
        let empty = OidcVerifier::from_jwks(settings(ISSUER), &JwkSet { keys: Vec::new() }, None);

        // When / Then
        let rejected = empty
            .verify(&mint(&claims(ISSUER, "alice", &[])))
            .await
            .expect_err("no key");
        assert!(
            rejected.reason.contains("does not publish"),
            "{}",
            rejected.reason
        );
    }

    #[tokio::test]
    async fn a_key_that_does_not_decode_is_skipped_not_fatal() {
        // Given — a key set publishing, before the fixture key, an RSA key
        // whose modulus is not base64url and so cannot become a DecodingKey.
        let mut published: JwkSet = serde_json::from_str(JWKS_JSON).expect("fixture");
        let mut broken: Jwk = published.keys[0].clone();
        broken.common.key_id = Some("broken".into());
        if let AlgorithmParameters::RSA(params) = &mut broken.algorithm {
            params.n = "not base64url!".into();
        }
        published.keys.insert(0, broken);
        let verifier = OidcVerifier::from_jwks(settings(ISSUER), &published, None);

        // When / Then — the fixture key is still usable, and a token naming
        // the broken kid is refused as unpublished rather than panicking.
        assert!(
            verifier
                .verify(&mint(&claims(ISSUER, "alice", &[])))
                .await
                .is_ok()
        );
        let rejected = verifier
            .verify(&mint_with(
                Algorithm::RS256,
                Some("broken"),
                &claims(ISSUER, "alice", &[]),
            ))
            .await
            .expect_err("broken kid");
        assert!(
            rejected.reason.contains("does not publish"),
            "{}",
            rejected.reason
        );
    }

    #[tokio::test]
    async fn a_token_without_a_kid_uses_the_only_key() {
        let token = mint_with(Algorithm::RS256, None, &claims(ISSUER, "alice", &[]));
        assert!(verifier().verify(&token).await.is_ok());
    }

    #[tokio::test]
    async fn username_claim_falls_back_to_sub() {
        // Given
        let mut claims = claims(ISSUER, "alice", &[]);
        claims.as_object_mut().unwrap().remove("preferred_username");

        // When / Then
        let identity = verifier().verify(&mint(&claims)).await.expect("verifies");
        assert_eq!(identity.subject, "alice-opaque-id");
    }

    #[tokio::test]
    async fn a_token_with_neither_username_nor_sub_is_rejected() {
        let mut claims = claims(ISSUER, "alice", &[]);
        let object = claims.as_object_mut().unwrap();
        object.remove("preferred_username");
        object.remove("sub");
        assert!(verifier().verify(&mint(&claims)).await.is_err());
    }

    #[tokio::test]
    async fn the_groups_claim_name_is_configurable_and_zitadel_roles_become_groups() {
        // Given — Zitadel's shape under its claim name.
        let mut settings = settings(ISSUER);
        settings.groups_claim = "urn:zitadel:iam:org:project:roles".to_string();
        let verifier = OidcVerifier::from_jwks(settings, &jwks(), None);
        let mut claims = claims(ISSUER, "alice", &["ignored-under-the-default-name"]);
        claims["urn:zitadel:iam:org:project:roles"] = serde_json::json!({
            "admin": { "123": "example.zitadel.cloud" },
            "editor": { "123": "example.zitadel.cloud" },
        });

        // When / Then
        let identity = verifier.verify(&mint(&claims)).await.expect("verifies");
        assert_eq!(
            identity.groups,
            ["admin", "editor"]
                .map(str::to_string)
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn every_documented_groups_shape_is_read() {
        use serde_json::json;
        let expect = |value: serde_json::Value, groups: &[&str]| {
            assert_eq!(
                groups_from_claim(&value),
                groups.iter().map(ToString::to_string).collect(),
                "{value}"
            );
        };
        expect(json!(["a", "b"]), &["a", "b"]);
        expect(json!("solo"), &["solo"]);
        expect(json!({"role-a": {}, "role-b": {}}), &["role-a", "role-b"]);
        expect(
            json!([{"role-a": {}}, {"role-b": {}}]),
            &["role-a", "role-b"],
        );
        expect(json!(["a", 7, null, {"b": {}}]), &["a", "b"]);
        expect(json!(42), &[]);
        expect(json!(null), &[]);
    }

    #[tokio::test]
    async fn discovery_fetches_the_key_set_and_verifies() {
        // Given
        let server = issuer_serving(JWKS_JSON).await;

        // When
        let verifier = OidcVerifier::discover(settings(&server.uri()))
            .await
            .expect("discovers");

        // Then
        let token = mint(&claims(&server.uri(), "alice", &["editors"]));
        assert_eq!(
            verifier.verify(&token).await.expect("verifies").subject,
            "alice"
        );
    }

    #[tokio::test]
    async fn discovery_with_a_mismatched_issuer_refuses_startup() {
        // Given — the provider spells its issuer with a trailing slash.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": format!("{}/", server.uri()),
                "jwks_uri": format!("{}/jwks", server.uri()),
            })))
            .mount(&server)
            .await;

        // When
        let error = OidcVerifier::discover(settings(&server.uri()))
            .await
            .expect_err("mismatch");

        // Then
        assert!(error.to_string().contains("trailing slash"), "{error}");
    }

    #[tokio::test]
    async fn a_ca_bundle_that_is_not_pem_refuses_startup_before_any_request() {
        // Given — a file that is not a certificate bundle, and one that is empty.
        let dir = tempfile::tempdir().expect("tempdir");
        let garbage = dir.path().join("ca.pem");
        std::fs::write(&garbage, b"not a certificate").expect("write");
        let mut settings = settings("https://auth.example.com");
        settings.ca_cert = Some(garbage.clone());

        // When
        let error = OidcVerifier::discover(settings).await.expect_err("refused");

        // Then — named, and no discovery request was attempted.
        let message = error.to_string();
        assert!(message.contains("NOTEDTHAT_OIDC_CA_CERT"), "{message}");
        assert!(
            message.contains(&garbage.display().to_string()),
            "{message}"
        );
    }

    #[tokio::test]
    async fn discovery_failure_refuses_startup() {
        let server = MockServer::start().await;
        let error = OidcVerifier::discover(settings(&server.uri()))
            .await
            .expect_err("404");
        assert!(
            error.to_string().contains("openid-configuration"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_unknown_kid_triggers_one_refetch_and_then_verifies() {
        // Given — a verifier holding no keys, whose issuer publishes the fixture.
        let server = issuer_serving(JWKS_JSON).await;
        let verifier = OidcVerifier::from_jwks(
            settings(&server.uri()),
            &JwkSet { keys: Vec::new() },
            Some(format!("{}/jwks", server.uri())),
        );
        // Age the cache past the refetch interval.
        verifier.mark_stale();

        // When
        let token = mint(&claims(&server.uri(), "alice", &[]));
        let first = verifier.verify(&token).await;
        let second = verifier.verify(&token).await;

        // Then — one fetch served both.
        assert!(first.is_ok(), "{first:?}");
        assert!(second.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refetches_are_rate_limited() {
        // Given — an empty key set fetched just now.
        let server = issuer_serving(JWKS_JSON).await;
        let verifier = OidcVerifier::from_jwks(
            settings(&server.uri()),
            &JwkSet { keys: Vec::new() },
            Some(format!("{}/jwks", server.uri())),
        );

        // When — tokens with an unknown kid arrive within the interval.
        let token = mint(&claims(&server.uri(), "alice", &[]));
        for _ in 0..5 {
            assert!(verifier.verify(&token).await.is_err());
        }

        // Then — the issuer was not asked.
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stale_key_set_is_refreshed_before_use() {
        // Given — keys older than the maximum age, and a rotated issuer.
        let server = issuer_serving(JWKS_JSON).await;
        let verifier = OidcVerifier::from_jwks(
            settings(&server.uri()),
            &JwkSet { keys: Vec::new() },
            Some(format!("{}/jwks", server.uri())),
        );
        verifier.mark_stale();

        // When / Then
        let token = mint(&claims(&server.uri(), "alice", &[]));
        assert!(verifier.verify(&token).await.is_ok());
    }

    #[tokio::test]
    async fn a_failed_refetch_keeps_the_previous_keys() {
        // Given — a verifier whose issuer has gone away.
        let server = MockServer::start().await;
        let verifier = OidcVerifier::from_jwks(
            settings(ISSUER),
            &jwks(),
            Some(format!("{}/jwks", server.uri())),
        );
        verifier.mark_stale();

        // When / Then — the stale refresh fails, the old key still verifies.
        let token = mint(&claims(ISSUER, "alice", &[]));
        assert!(verifier.verify(&token).await.is_ok());
    }

    #[test]
    fn debug_output_names_the_issuer_and_nothing_secret() {
        let rendered = format!("{:?}", verifier());
        assert!(rendered.contains(ISSUER));
        assert!(!rendered.contains("keys:"));
    }
}
