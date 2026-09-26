//! HTTP client wrapping reqwest for `NotedThat` API access.

use crate::error::{McpToolError, map_response};
use bytes::Bytes;
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;
use url::Url;

/// The default read budget: what one object read may fetch from the API.
///
/// Equal to the API's own body cap, so anything written *through* the API can be
/// read back whole. Only objects that arrived some other way — a `WebDAV` `PUT`, a
/// file dropped into an `fs` tree — can be larger, and those are read in slices.
pub const DEFAULT_MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

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

/// Async HTTP client for the `NotedThat` API.
///
/// Clone-cheap: clones share the underlying reqwest connection pool.
#[derive(Clone, Debug)]
pub struct NotedThatClient {
    pub(crate) http: reqwest::Client,
    /// For the event streams the subscription forwarders hold open (D66): no
    /// total timeout, a connect timeout, and a read timeout of three missed
    /// 15 s heartbeats so a dead connection is noticed.
    stream_http: reqwest::Client,
    base_url: Url,
    /// The bearer every request carries, or `None` for the anonymous caller,
    /// whose requests carry no `Authorization` header at all.
    pub(crate) token: Option<String>,
    /// Most bytes one object read may fetch; see [`Self::read_body_bounded`].
    pub(crate) max_read_bytes: u64,
    /// The deadline [`Self::http`] was built with; see [`Self::api_timeout`].
    api_timeout: Duration,
}

/// The server request timeout this client assumes when nothing tells it one:
/// `NOTEDTHAT_REQUEST_TIMEOUT_MS`'s own default. A client built by
/// [`NotedThatClient::new`] alone — every test constructor — then keeps the
/// same relationship to it that [`NotedThatClient::with_api_timeout`] gives a
/// configured one.
const DEFAULT_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The tool-call client's deadline when nothing overrides it.
const DEFAULT_API_TIMEOUT: Duration = api_timeout_from(DEFAULT_SERVER_REQUEST_TIMEOUT);

/// How far the tool-call client's deadline sits beyond the server's, so the
/// API's own `504` is what answers a stalled call rather than a transport
/// error from this side. Covers connect, the request head and auth, all of
/// which run before the server's clock starts.
const API_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);

/// The tool-call deadline for a server whose request timeout is
/// `request_timeout`. Always longer than it: that ordering is the whole point,
/// and `the_tool_call_deadline_always_outlasts_the_server_timeout` pins it.
const fn api_timeout_from(request_timeout: Duration) -> Duration {
    request_timeout.saturating_add(API_TIMEOUT_MARGIN)
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
            .timeout(DEFAULT_API_TIMEOUT)
            .build()
            .map_err(ConfigError::ClientBuild)?;
        let stream_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(45))
            .build()
            .map_err(ConfigError::ClientBuild)?;

        Ok(Self {
            http,
            stream_http,
            base_url: parsed,
            token: Some(token_trimmed.to_string()),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            api_timeout: DEFAULT_API_TIMEOUT,
        })
    }

    /// Give the tool-call client a deadline derived from the server's own.
    ///
    /// Every tool call reaches the API over loopback on the same listener, so
    /// the API's request timeout is what should answer a stalled one — as a
    /// `504`, which `map_response` turns into a `request_timeout` tool error
    /// (D70). That only happens if this client is still waiting when it
    /// arrives, and its clock starts first: before connect, before the head is
    /// sent, before auth, while the server's starts when `bound` runs. Equal
    /// deadlines therefore go to `reqwest`, and the tool reports a transport
    /// error instead — and an operator who raises
    /// `NOTEDTHAT_REQUEST_TIMEOUT_MS` for a slow embedder would find tool
    /// calls still cut off at 30 s.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ClientBuild`] if the HTTP client cannot be built.
    pub fn with_api_timeout(mut self, request_timeout: Duration) -> Result<Self, ConfigError> {
        self.api_timeout = api_timeout_from(request_timeout);
        self.http = reqwest::Client::builder()
            .timeout(self.api_timeout)
            .build()
            .map_err(ConfigError::ClientBuild)?;
        Ok(self)
    }

    /// The deadline one tool call's API request is given.
    ///
    /// Exposed because it is a derived value with a contract — it must outlast
    /// the server request timeout it came from — and the caller that derives
    /// it, `run::mcp_http`, is the only place that can be checked.
    #[must_use]
    pub fn api_timeout(&self) -> Duration {
        self.api_timeout
    }

    /// Cap what one object read may fetch from the API, in bytes.
    ///
    /// Zero is not a budget; callers validate that before getting here.
    #[must_use]
    pub fn with_max_read_bytes(mut self, max_read_bytes: u64) -> Self {
        self.max_read_bytes = max_read_bytes;
        self
    }

    /// The same client — same connection pool, same base URL — presenting
    /// `token` instead.
    ///
    /// How the MCP service acts as its caller: the service is built once with
    /// the server's own token, and each tool call swaps in the credential the
    /// caller presented. Not validated here; the API validates it on arrival.
    #[must_use]
    pub fn with_token(&self, token: &str) -> Self {
        Self {
            http: self.http.clone(),
            stream_http: self.stream_http.clone(),
            base_url: self.base_url.clone(),
            token: Some(token.to_string()),
            max_read_bytes: self.max_read_bytes,
            api_timeout: self.api_timeout,
        }
    }

    /// The same client presenting no credential at all.
    ///
    /// How the MCP service acts as an anonymous caller: the loopback call
    /// carries no `Authorization` header, the API resolves it to the anonymous
    /// principal, and the manifests' `anyone` rules decide — exactly as they
    /// would for an anonymous request made directly.
    #[must_use]
    pub fn anonymous(&self) -> Self {
        Self {
            http: self.http.clone(),
            stream_http: self.stream_http.clone(),
            base_url: self.base_url.clone(),
            token: None,
            max_read_bytes: self.max_read_bytes,
            api_timeout: self.api_timeout,
        }
    }

    /// The credential this client presents, for keying per-caller state by.
    ///
    /// `None` is the anonymous caller, which is a credential like any other
    /// here: what the API resolves it to and what D51 then grants it differ
    /// from every bearer's, so state opened as anonymous must not be shared
    /// with state opened as anyone else. Used only as a map key inside one
    /// session; never logged or rendered.
    #[must_use]
    pub(crate) fn credential(&self) -> Option<String> {
        self.token.clone()
    }

    /// Returns the base URL as a display string (for logging — safe, no token).
    pub fn base_url_display(&self) -> &str {
        self.base_url.as_str()
    }

    /// Build a URL for the API: `<base>/api/v1/<segments joined with '/'>`.
    pub(crate) fn api_v1_url(&self, path_segments: &[&str]) -> Url {
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().unwrap_or_else(|()| {
                unreachable!("validated http/https URL always supports path segments")
            });
            segments.push("api");
            segments.push("v1");
            for seg in path_segments {
                segments.push(seg);
            }
        }
        url
    }

    /// The knowledge base's object change events (D55), as this caller, from
    /// after `last_event_id` when there is one — the stream a subscription
    /// forwarder consumes (D66). Sent on the client without a total timeout.
    pub(crate) fn events_stream(
        &self,
        kb_slug: &str,
        last_event_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let url = self.api_v1_url(&["knowledgebases", kb_slug, "events"]);
        let mut request = self
            .stream_http
            .get(url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }
        self.authorized(request)
    }

    /// Attach `Authorization: Bearer <token>` to a request — nothing for the
    /// anonymous caller — and name this surface so a write's change event says
    /// `mcp` rather than `http`.
    pub(crate) fn authorized(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        };
        req.header(SOURCE_HEADER, SOURCE_VALUE)
    }

    /// The body of a successful response, within this client's read budget.
    ///
    /// A `Content-Length` over the budget is refused before a byte of body is
    /// read. Without one — or with a misleading one — the body is read chunk by
    /// chunk and refused the moment it exceeds the budget, so the most this ever
    /// holds is the budget plus one chunk. Either refusal is
    /// [`McpToolError::ResponseTooLarge`], which names the slice arguments.
    ///
    /// Inspect headers *before* calling this: it consumes the response.
    pub(crate) async fn read_body_bounded(
        &self,
        mut resp: reqwest::Response,
    ) -> Result<Bytes, McpToolError> {
        let limit = self.max_read_bytes;
        let declared = resp.content_length();
        if let Some(total) = declared
            && total > limit
        {
            return Err(McpToolError::ResponseTooLarge {
                limit,
                total: Some(total),
            });
        }

        let capacity = declared
            .unwrap_or(0)
            .min(limit)
            .try_into()
            .unwrap_or(usize::MAX);
        let mut body = Vec::with_capacity(capacity);
        while let Some(chunk) = resp.chunk().await? {
            let len = u64::try_from(body.len() + chunk.len()).unwrap_or(u64::MAX);
            if len > limit {
                return Err(McpToolError::ResponseTooLarge {
                    limit,
                    total: declared,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(body))
    }

    /// The knowledge bases visible to this client's caller, in the order the
    /// API lists them: `GET /api/v1/knowledgebases`.
    ///
    /// Shared by `list_knowledgebases`, `resources/list` and a `search` with
    /// no `kb` so that all three discover the same set.
    pub(crate) async fn list_kbs(&self) -> Result<Vec<KbSummary>, McpToolError> {
        /// An entry as the API spells it: an object since #98, a bare slug
        /// before. This client only ever talks to the server it is mounted in,
        /// so the bare form is not expected on the wire — but listing is the
        /// first thing every client does, and tolerating both shapes costs
        /// nothing while an opaque `expected struct KbSummary` would be the
        /// wrong way to learn of a listing-shape change.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Entry {
            Summary(KbSummary),
            Slug(String),
        }

        #[derive(Deserialize)]
        struct ListKbsResponse {
            knowledgebases: Vec<Entry>,
        }

        let url = self.api_v1_url(&["knowledgebases"]);
        let resp = self.authorized(self.http.get(url)).send().await?;
        let resp = map_response(resp).await?;
        let body: ListKbsResponse = resp.json().await?;
        Ok(body
            .knowledgebases
            .into_iter()
            .map(|entry| match entry {
                Entry::Summary(summary) => summary,
                Entry::Slug(kb_slug) => KbSummary {
                    kb_slug,
                    display_name: None,
                    description: None,
                },
            })
            .collect())
    }

    /// Only the slugs of [`Self::list_kbs`], for the callers that address
    /// knowledge bases without describing them.
    pub(crate) async fn list_kb_slugs(&self) -> Result<Vec<String>, McpToolError> {
        Ok(self
            .list_kbs()
            .await?
            .into_iter()
            .map(|kb| kb.kb_slug)
            .collect())
    }
}

/// One entry of `GET /api/v1/knowledgebases` (#98).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct KbSummary {
    /// The slug every route takes.
    pub(crate) kb_slug: String,
    /// The manifest's human-readable name. `None` only for an entry a server
    /// from before #98 sent as a bare slug; `list_knowledgebases` shows the
    /// slug then, as the server itself does for a manifest without a name.
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    /// What the knowledge base is for, when its manifest says.
    #[serde(default)]
    pub(crate) description: Option<String>,
}

/// The header that attributes a write to this surface in its change event.
/// Informational: the API grants nothing on its account.
pub const SOURCE_HEADER: &str = "x-notedthat-source";
/// The value the header carries.
pub const SOURCE_VALUE: &str = "mcp";

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing the derivation must guarantee: whatever the server's
    /// request timeout is, this client is still waiting when the server's own
    /// `504` arrives. Equal deadlines are not enough — the client's clock
    /// starts before connect, the head and auth, the server's when `bound`
    /// runs — so the margin is what makes a stalled tool call a
    /// `request_timeout` rather than a transport error (D70).
    #[test]
    fn the_tool_call_deadline_always_outlasts_the_server_timeout() {
        for request_timeout in [
            Duration::ZERO,
            Duration::from_millis(1),
            DEFAULT_SERVER_REQUEST_TIMEOUT,
            Duration::from_secs(600),
            Duration::MAX,
        ] {
            assert!(
                api_timeout_from(request_timeout) >= request_timeout,
                "a {request_timeout:?} server timeout derives a shorter deadline"
            );
        }
        // Saturating at `Duration::MAX` is the one case where the two are
        // equal; every reachable setting has room for the margin.
        assert!(DEFAULT_API_TIMEOUT > DEFAULT_SERVER_REQUEST_TIMEOUT);
        assert!(api_timeout_from(Duration::from_secs(600)) > Duration::from_secs(600));
    }

    /// A client built without [`NotedThatClient::with_api_timeout`] — every
    /// test constructor, and anything that forgets — is still no shorter than
    /// a server left on its default.
    #[test]
    fn the_default_client_deadline_outlasts_the_default_server_timeout() {
        let client = NotedThatClient::new("http://localhost:8080", "tok").unwrap();
        assert_eq!(client.api_timeout(), DEFAULT_API_TIMEOUT);
        assert!(client.api_timeout() > DEFAULT_SERVER_REQUEST_TIMEOUT);
    }

    #[test]
    fn with_api_timeout_derives_from_the_server_timeout_it_is_given() {
        let client = NotedThatClient::new("http://localhost:8080", "tok")
            .unwrap()
            .with_api_timeout(Duration::from_secs(90))
            .unwrap();
        assert_eq!(
            client.api_timeout(),
            api_timeout_from(Duration::from_secs(90))
        );
        // And it survives the per-caller clones the MCP service makes.
        assert_eq!(
            client.with_token("other").api_timeout(),
            client.api_timeout()
        );
        assert_eq!(client.anonymous().api_timeout(), client.api_timeout());
    }

    #[test]
    fn url_normalization_trailing_slash_stripped() {
        let c = NotedThatClient::new("http://localhost:8080/", "tok").unwrap();
        // api_v1_url should work and NOT double-slash
        let u = c.api_v1_url(&["knowledgebases"]);
        assert_eq!(u.as_str(), "http://localhost:8080/api/v1/knowledgebases");
    }

    #[test]
    fn url_normalization_no_trailing_slash() {
        let c = NotedThatClient::new("http://localhost:8080", "tok").unwrap();
        let u = c.api_v1_url(&["knowledgebases", "notes"]);
        assert_eq!(
            u.as_str(),
            "http://localhost:8080/api/v1/knowledgebases/notes"
        );
    }

    #[test]
    fn every_request_names_this_surface_and_carries_the_bearer() {
        let c = NotedThatClient::new("http://localhost:8080", "tok").unwrap();
        let req = c
            .authorized(c.http.get(c.api_v1_url(&["knowledgebases"])))
            .build()
            .expect("request builds");
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer tok");
        assert_eq!(req.headers().get(SOURCE_HEADER).unwrap(), SOURCE_VALUE);
    }

    #[test]
    fn an_anonymous_client_sends_no_authorization_header() {
        // Given: a client acting as the anonymous caller
        let c = NotedThatClient::new("http://localhost:8080", "tok")
            .unwrap()
            .anonymous();

        // When: it builds a request
        let req = c
            .authorized(c.http.get(c.api_v1_url(&["knowledgebases"])))
            .build()
            .expect("request builds");

        // Then: no credential at all — the API must see an anonymous request,
        // not the server's own token — while the surface is still named
        assert!(req.headers().get("authorization").is_none());
        assert_eq!(req.headers().get(SOURCE_HEADER).unwrap(), SOURCE_VALUE);
    }

    #[test]
    fn the_read_budget_defaults_to_the_api_body_cap_and_survives_a_token_swap() {
        let c = NotedThatClient::new("http://localhost:8080", "tok").unwrap();
        assert_eq!(c.max_read_bytes, DEFAULT_MAX_READ_BYTES);
        assert_eq!(DEFAULT_MAX_READ_BYTES, 16 * 1024 * 1024);
        let c = c.with_max_read_bytes(4096);
        assert_eq!(c.with_token("other").max_read_bytes, 4096);
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
    fn api_v1_url_encodes_raw_object_path_exactly_once() {
        // Characterization test: api_v1_url with raw object path input already encodes
        // exactly once. This is NOT the red gate — it passes before AND after the fix.
        // It documents that the api_v1_url function itself is correct; the bug was
        // in the tool code pre-encoding before calling api_v1_url.
        let c = NotedThatClient::new("http://localhost:8080", "tok").unwrap();

        // Raw nested path → exactly one encoding pass
        let u = c.api_v1_url(&["knowledgebases", "notes", "docs/rfc/7231.md"]);
        let path = u.path();
        assert_eq!(
            path, "/api/v1/knowledgebases/notes/docs%2Frfc%2F7231.md",
            "raw nested path should be encoded exactly once"
        );
        assert!(
            !path.contains("%25"),
            "no double-encoding: %25 must not appear in path: {path}"
        );

        // Literal percent in input → encoded to %25, never %2525
        let u2 = c.api_v1_url(&["knowledgebases", "notes", "a%b.md"]);
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
