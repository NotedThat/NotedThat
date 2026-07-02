//! Raw HTTP conditional request headers as received from the client.
//!
//! Per SPECIFICATIONS.md D9, `NotedThat` forwards these values verbatim to the S3 backend
//! without validation. Each field holds the raw header value as a string; the S3 adapter
//! performs any parsing (e.g., HTTP-date conversion for `If-*-Since`).

/// Raw HTTP conditional request headers as received from the client.
///
/// This struct is a pure carrier per SPECIFICATIONS.md D9 — `NotedThat` forwards
/// these values verbatim to the S3 backend without validation. Each field holds
/// the raw header value as a string; the S3 adapter performs any parsing
/// (e.g., HTTP-date conversion for `If-*-Since`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalHeaders {
    /// Value of the `If-Match` header, if present.
    pub if_match: Option<String>,
    /// Value of the `If-None-Match` header, if present.
    pub if_none_match: Option<String>,
    /// Value of the `If-Modified-Since` header, if present (raw HTTP-date string).
    pub if_modified_since: Option<String>,
    /// Value of the `If-Unmodified-Since` header, if present (raw HTTP-date string).
    pub if_unmodified_since: Option<String>,
}

impl ConditionalHeaders {
    /// Returns true if all four conditional header fields are None.
    pub fn is_empty(&self) -> bool {
        self.if_match.is_none()
            && self.if_none_match.is_none()
            && self.if_modified_since.is_none()
            && self.if_unmodified_since.is_none()
    }

    /// Extracts the four conditional headers from an HTTP `HeaderMap`.
    ///
    /// Non-UTF-8 header values are silently dropped (treated as missing).
    pub fn from_header_map(headers: &http::HeaderMap) -> Self {
        Self {
            if_match: headers
                .get(http::header::IF_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            if_none_match: headers
                .get(http::header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            if_modified_since: headers
                .get(http::header::IF_MODIFIED_SINCE)
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            if_unmodified_since: headers
                .get(http::header::IF_UNMODIFIED_SINCE)
                .and_then(|v| v.to_str().ok())
                .map(String::from),
        }
    }
}

// Compile-time trait bound check: ConditionalHeaders must be Send + Sync + Clone + Debug
fn _assert_bounds()
where
    ConditionalHeaders: Send + Sync + Clone + std::fmt::Debug,
{
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_all_none() {
        let headers = ConditionalHeaders::default();
        assert_eq!(headers.if_match, None);
        assert_eq!(headers.if_none_match, None);
        assert_eq!(headers.if_modified_since, None);
        assert_eq!(headers.if_unmodified_since, None);
    }

    #[test]
    fn test_default_is_empty() {
        let headers = ConditionalHeaders::default();
        assert!(headers.is_empty());
    }

    #[test]
    fn test_is_empty_false_with_if_match() {
        let headers = ConditionalHeaders {
            if_match: Some("\"abc123\"".to_string()),
            if_none_match: None,
            if_modified_since: None,
            if_unmodified_since: None,
        };
        assert!(!headers.is_empty());
    }

    #[test]
    fn test_is_empty_false_with_if_none_match() {
        let headers = ConditionalHeaders {
            if_match: None,
            if_none_match: Some("\"xyz789\"".to_string()),
            if_modified_since: None,
            if_unmodified_since: None,
        };
        assert!(!headers.is_empty());
    }

    #[test]
    fn test_from_header_map_extracts_if_match() {
        let mut map = http::HeaderMap::new();
        map.insert(
            http::header::IF_MATCH,
            http::HeaderValue::from_static("\"etag-value\""),
        );
        let headers = ConditionalHeaders::from_header_map(&map);
        assert_eq!(headers.if_match, Some("\"etag-value\"".to_string()));
        assert_eq!(headers.if_none_match, None);
        assert_eq!(headers.if_modified_since, None);
        assert_eq!(headers.if_unmodified_since, None);
    }

    #[test]
    fn test_from_header_map_missing_headers() {
        let map = http::HeaderMap::new();
        let headers = ConditionalHeaders::from_header_map(&map);
        assert_eq!(headers, ConditionalHeaders::default());
    }

    #[test]
    fn test_from_header_map_empty_map_is_empty() {
        let map = http::HeaderMap::new();
        let headers = ConditionalHeaders::from_header_map(&map);
        assert!(headers.is_empty());
    }

    #[test]
    fn test_from_header_map_all_four_headers() {
        let mut map = http::HeaderMap::new();
        map.insert(
            http::header::IF_MATCH,
            http::HeaderValue::from_static("\"match-etag\""),
        );
        map.insert(
            http::header::IF_NONE_MATCH,
            http::HeaderValue::from_static("\"none-match-etag\""),
        );
        map.insert(
            http::header::IF_MODIFIED_SINCE,
            http::HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        map.insert(
            http::header::IF_UNMODIFIED_SINCE,
            http::HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );

        let headers = ConditionalHeaders::from_header_map(&map);
        assert_eq!(headers.if_match, Some("\"match-etag\"".to_string()));
        assert_eq!(
            headers.if_none_match,
            Some("\"none-match-etag\"".to_string())
        );
        assert_eq!(
            headers.if_modified_since,
            Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string())
        );
        assert_eq!(
            headers.if_unmodified_since,
            Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string())
        );
        assert!(!headers.is_empty());
    }

    #[test]
    fn test_trait_bounds_send_sync_clone_debug() {
        // This test verifies at compile time that ConditionalHeaders implements
        // Send, Sync, Clone, and Debug. If it doesn't, this won't compile.
        fn assert_send_sync_clone_debug<T: Send + Sync + Clone + std::fmt::Debug>() {}
        assert_send_sync_clone_debug::<ConditionalHeaders>();
    }
}
