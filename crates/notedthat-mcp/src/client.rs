//! HTTP client wrapping reqwest for `NotedThat` API access.

use thiserror::Error;
use url::Url;

/// Errors from constructing a [`NotedThatClient`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The URL string could not be parsed.
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    /// The URL has a non-http/https scheme.
    #[error("URL must use http or https scheme (got: {0})")]
    UrlNotHttp(String),
    /// The token is empty (after trimming whitespace).
    #[error("token must not be empty")]
    EmptyToken,
    /// The reqwest HTTP client could not be constructed (e.g. TLS unavailable).
    #[error("could not build HTTP client: {0}")]
    ClientBuild(reqwest::Error),
}

/// Async HTTP client for the `NotedThat` v1 API.
///
/// Clone-cheap: clones share the underlying reqwest connection pool.
#[derive(Clone, Debug)]
pub struct NotedThatClient {
    pub(crate) http: reqwest::Client,
    base_url: Url,
    token: String,
}

impl NotedThatClient {
    /// Create a new client.
    ///
    /// - Trims `url`, strips trailing `/`, parses via [`url::Url::parse`].
    /// - Rejects non-http/https schemes.
    /// - Trims `token`, rejects empty-after-trim.
    pub fn new(url: &str, token: &str) -> Result<Self, ConfigError> {
        // Trim and strip trailing slash from URL
        let url_trimmed = url.trim().trim_end_matches('/');
        let parsed = Url::parse(url_trimmed)?;

        // Enforce http/https
        let scheme = parsed.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(ConfigError::UrlNotHttp(scheme.to_string()));
        }

        // Trim and validate token
        let token_trimmed = token.trim();
        if token_trimmed.is_empty() {
            return Err(ConfigError::EmptyToken);
        }

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(ConfigError::ClientBuild)?;

        Ok(Self {
            http,
            base_url: parsed,
            token: token_trimmed.to_string(),
        })
    }

    /// Returns the base URL as a display string (for logging — safe, no token).
    pub fn base_url_display(&self) -> &str {
        self.base_url.as_str()
    }

    /// Build a URL for the v1 API: `<base>/v1/<segments joined with '/'>`.
    pub(crate) fn v1_url(&self, path_segments: &[&str]) -> Url {
        let mut url = self.base_url.clone();
        {
        let mut segments = url
            .path_segments_mut()
            .unwrap_or_else(|()| unreachable!("validated http/https URL always supports path segments"));
            segments.push("v1");
            for seg in path_segments {
                segments.push(seg);
            }
        }
        url
    }

    /// Attach `Authorization: Bearer <token>` header to a request.
    pub(crate) fn authorized(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization_trailing_slash_stripped() {
        let c = NotedThatClient::new("http://localhost:8080/", "tok").unwrap();
        // v1_url should work and NOT double-slash
        let u = c.v1_url(&["knowledgebases"]);
        assert_eq!(u.as_str(), "http://localhost:8080/v1/knowledgebases");
    }

    #[test]
    fn url_normalization_no_trailing_slash() {
        let c = NotedThatClient::new("http://localhost:8080", "tok").unwrap();
        let u = c.v1_url(&["knowledgebases", "notes"]);
        assert_eq!(u.as_str(), "http://localhost:8080/v1/knowledgebases/notes");
    }

    #[test]
    fn invalid_url_rejected() {
        let r = NotedThatClient::new("not-a-url", "tok");
        assert!(r.is_err(), "non-URL should be rejected");
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("invalid") || err.contains("URL") || err.contains("parse"),
            "error message should mention URL/parse: {err}"
        );
    }

    #[test]
    fn empty_token_rejected() {
        let r = NotedThatClient::new("http://localhost", "");
        assert!(r.is_err());
        assert!(matches!(r.unwrap_err(), ConfigError::EmptyToken));
    }

    #[test]
    fn whitespace_token_rejected() {
        let r = NotedThatClient::new("http://localhost", "   ");
        assert!(r.is_err());
        assert!(matches!(r.unwrap_err(), ConfigError::EmptyToken));
    }

    #[test]
    fn wrong_scheme_rejected() {
        let r = NotedThatClient::new("ftp://example.com", "tok");
        assert!(r.is_err());
        assert!(matches!(r.unwrap_err(), ConfigError::UrlNotHttp(_)));
    }

    #[test]
    fn send_sync_clone() {
        // Compile-time assertion that NotedThatClient is Send + Sync + Clone
        const _: fn() = || {
            fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
            assert_send_sync_clone::<NotedThatClient>();
        };
    }

    #[test]
    fn v1_url_encodes_raw_object_path_exactly_once() {
        // Characterization test: v1_url with raw object path input already encodes
        // exactly once. This is NOT the red gate — it passes before AND after the fix.
        // It documents that the v1_url function itself is correct; the bug was
        // in the tool code pre-encoding before calling v1_url.
        let c = NotedThatClient::new("http://localhost:8080", "tok").unwrap();

        // Raw nested path → exactly one encoding pass
        let u = c.v1_url(&["knowledgebases", "notes", "docs/rfc/7231.md"]);
        let path = u.path();
        assert_eq!(
            path,
            "/v1/knowledgebases/notes/docs%2Frfc%2F7231.md",
            "raw nested path should be encoded exactly once"
        );
        assert!(
            !path.contains("%25"),
            "no double-encoding: %25 must not appear in path: {path}"
        );

        // Literal percent in input → encoded to %25, never %2525
        let u2 = c.v1_url(&["knowledgebases", "notes", "a%b.md"]);
        let path2 = u2.path();
        assert!(
            path2.contains("%25b"),
            "literal % must encode to %25b: {path2}"
        );
        assert!(
            !path2.contains("%2525"),
            "literal % must not double-encode to %2525: {path2}"
        );
    }
}
