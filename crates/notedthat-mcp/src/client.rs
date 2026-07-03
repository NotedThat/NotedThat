//! HTTP client wrapping reqwest for NotedThat API access.

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
}

/// Async HTTP client for the NotedThat v1 API.
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
            .expect("reqwest client construction should not fail");

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
                .expect("base URL must be able to have path segments");
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
}
