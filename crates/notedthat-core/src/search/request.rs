use serde::{Deserialize, Serialize};

use super::{SearchError, SearchFilter};

/// Maximum byte length of the query string (UTF-8 bytes, not chars).
pub const MAX_QUERY_BYTES: usize = 8_192;

/// Default number of hits to return when `limit` is not specified.
pub const DEFAULT_LIMIT: u32 = 10;

/// Maximum number of hits the client can request.
pub const MAX_LIMIT: u32 = 50;

/// Minimum valid limit value (explicit 0 is rejected).
pub const MIN_LIMIT: u32 = 1;

/// Raw (unvalidated) search request body.
///
/// `#[non_exhaustive]` is NOT applied to request types — it would break
/// struct-literal construction in tests and client code.
/// Unknown JSON fields are silently ignored for forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The natural-language query string (required).
    pub query: String,

    /// Optional filter conditions (AND-composed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<SearchFilter>,

    /// Maximum number of hits to return. Clamped to `[1, 50]`. Defaults to `10`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A `SearchRequest` that has been validated.
///
/// The Searcher trait accepts `ValidatedRequest`, making it a compile-time
/// error to call `Searcher::search` without first running `SearchRequest::validate()`.
#[derive(Debug, Clone)]
pub struct ValidatedRequest {
    /// The trimmed, non-empty query string.
    pub query: String,
    /// The optional (already-valid) filter.
    pub filter: Option<SearchFilter>,
    /// The clamped limit in `[1, 50]`.
    pub limit: u32,
}

impl SearchRequest {
    /// Validate this request, returning a `ValidatedRequest` on success.
    ///
    /// Validation rules:
    /// - `query` trimmed of whitespace must be non-empty.
    /// - `query` byte length must not exceed `MAX_QUERY_BYTES`.
    /// - `limit` must not be `Some(0)` (explicit zero is rejected; `None` → default 10).
    /// - `limit > MAX_LIMIT` is silently clamped to `MAX_LIMIT` (not an error).
    pub fn validate(self) -> Result<ValidatedRequest, SearchError> {
        let query = self.query.trim().to_string();

        if query.is_empty() {
            return Err(SearchError::invalid_input("query must not be blank"));
        }

        if query.len() > MAX_QUERY_BYTES {
            return Err(SearchError::invalid_input(format!(
                "query exceeds maximum length of {MAX_QUERY_BYTES} bytes"
            )));
        }

        let limit = match self.limit {
            None => DEFAULT_LIMIT,
            Some(0) => {
                return Err(SearchError::invalid_input(
                    "limit must be at least 1; use 'null' or omit to use the default",
                ));
            }
            Some(n) => n.min(MAX_LIMIT),
        };

        Ok(ValidatedRequest {
            query,
            filter: self.filter,
            limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            filter: None,
            limit: None,
        }
    }

    #[test]
    fn valid_request_passes() {
        let r = req("install cargo").validate();
        assert!(r.is_ok());
        let v = r.unwrap();
        assert_eq!(v.query, "install cargo");
        assert_eq!(v.limit, DEFAULT_LIMIT);
        assert!(v.filter.is_none());
    }

    #[test]
    fn empty_query_rejected() {
        assert!(matches!(
            req("").validate(),
            Err(SearchError::InvalidInput { .. })
        ));
    }

    #[test]
    fn whitespace_only_query_rejected() {
        assert!(matches!(
            req("   ").validate(),
            Err(SearchError::InvalidInput { .. })
        ));
    }

    #[test]
    fn query_trimmed_before_validation() {
        let v = req("  hello  ").validate().unwrap();
        assert_eq!(v.query, "hello");
    }

    #[test]
    fn query_exactly_8192_bytes_accepted() {
        let q = "a".repeat(8192);
        assert!(req(&q).validate().is_ok());
    }

    #[test]
    fn query_8193_bytes_rejected() {
        let q = "a".repeat(8193);
        assert!(matches!(
            req(&q).validate(),
            Err(SearchError::InvalidInput { .. })
        ));
    }

    #[test]
    fn limit_none_defaults_to_10() {
        let v = SearchRequest {
            query: "x".into(),
            filter: None,
            limit: None,
        }
        .validate()
        .unwrap();
        assert_eq!(v.limit, 10);
    }

    #[test]
    fn limit_zero_rejected() {
        let r = SearchRequest {
            query: "x".into(),
            filter: None,
            limit: Some(0),
        }
        .validate();
        assert!(matches!(r, Err(SearchError::InvalidInput { .. })));
    }

    #[test]
    fn limit_999_clamped_to_50() {
        let v = SearchRequest {
            query: "x".into(),
            filter: None,
            limit: Some(999),
        }
        .validate()
        .unwrap();
        assert_eq!(v.limit, 50);
    }

    #[test]
    fn limit_50_accepted() {
        let v = SearchRequest {
            query: "x".into(),
            filter: None,
            limit: Some(50),
        }
        .validate()
        .unwrap();
        assert_eq!(v.limit, 50);
    }

    #[test]
    fn limit_1_accepted() {
        let v = SearchRequest {
            query: "x".into(),
            filter: None,
            limit: Some(1),
        }
        .validate()
        .unwrap();
        assert_eq!(v.limit, 1);
    }

    #[test]
    fn serde_minimal_deserialization() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":"hello"}"#).unwrap();
        assert_eq!(r.query, "hello");
        assert!(r.filter.is_none());
        assert!(r.limit.is_none());
    }

    #[test]
    fn serde_unknown_fields_ignored() {
        let r: SearchRequest =
            serde_json::from_str(r#"{"query":"hello","extra":"ignored"}"#).unwrap();
        assert_eq!(r.query, "hello");
    }

    #[test]
    fn serde_negative_limit_fails_deserialization() {
        // u32 rejects negative integers at deserialize time
        let r: Result<SearchRequest, _> = serde_json::from_str(r#"{"query":"hello","limit":-1}"#);
        assert!(r.is_err());
    }

    #[test]
    fn serde_full_request_round_trip() {
        use super::super::SearchFilter;

        let original = SearchRequest {
            query: "test query".into(),
            filter: Some(SearchFilter {
                mime: Some("text/markdown".into()),
                ..Default::default()
            }),
            limit: Some(5),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: SearchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.query, "test query");
        assert_eq!(back.limit, Some(5));
    }
}
