//! HTTP status → MCP error mapping per §6.12.

use rmcp::model::{ErrorCode, ErrorData};
use thiserror::Error;

/// MCP tool-level errors.
#[derive(Debug, Error)]
pub enum McpToolError {
    /// 400 — invalid request parameters.
    #[error("invalid_request: {0}")]
    InvalidRequest(String),
    /// 401 — missing or invalid bearer token.
    #[error("unauthorized")]
    Unauthorized,
    /// 404 — knowledge base or object not found.
    #[error("not_found: {0}")]
    NotFound(String),
    /// 412 — If-Match or If-None-Match precondition failed.
    #[error("precondition_failed")]
    PreconditionFailed,
    /// 413 — request body exceeds server limit.
    #[error("payload_too_large")]
    PayloadTooLarge,
    /// 416 — Range header not satisfiable (RFC 7233 §4.4 — body is EMPTY).
    #[error("range_not_satisfiable")]
    RangeNotSatisfiable,
    /// Replacement `old_string` did not occur in the target text.
    #[error("no match found for old_string: {0}")]
    NoMatch(String),
    /// Replacement `old_string` occurred multiple times without `replace_all`.
    #[error("multiple matches ({count}); use replace_all to replace them all")]
    AmbiguousMatch {
        /// Number of matches found for the replacement `old_string`.
        count: u64,
    },
    /// 503 — backend (S3 / Qdrant) unavailable.
    #[error("backend_unavailable")]
    BackendUnavailable,
    /// 500 / other — internal server error.
    #[error("internal_error: {0}")]
    InternalError(String),
    /// Transport-level reqwest error.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// JSON deserialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl From<McpToolError> for ErrorData {
    fn from(e: McpToolError) -> Self {
        match e {
            McpToolError::InvalidRequest(msg) => {
                ErrorData::new(ErrorCode::INVALID_PARAMS, msg, None)
            }
            McpToolError::Unauthorized => {
                ErrorData::new(ErrorCode::INVALID_PARAMS, "unauthorized", None)
            }
            McpToolError::NotFound(msg) => ErrorData::new(
                ErrorCode::RESOURCE_NOT_FOUND,
                format!("not_found: {msg}"),
                None,
            ),
            McpToolError::PreconditionFailed => {
                ErrorData::new(ErrorCode::INVALID_PARAMS, "precondition_failed", None)
            }
            McpToolError::PayloadTooLarge => {
                ErrorData::new(ErrorCode::INVALID_PARAMS, "payload_too_large", None)
            }
            McpToolError::RangeNotSatisfiable => {
                ErrorData::new(ErrorCode::INVALID_PARAMS, "range_not_satisfiable", None)
            }
            McpToolError::NoMatch(old_string) => ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("no match found for old_string: {old_string}"),
                None,
            ),
            McpToolError::AmbiguousMatch { count } => ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("multiple matches ({count}); use replace_all to replace them all"),
                None,
            ),
            McpToolError::BackendUnavailable => {
                ErrorData::new(ErrorCode::INTERNAL_ERROR, "backend_unavailable", None)
            }
            McpToolError::InternalError(msg) => {
                ErrorData::new(ErrorCode::INTERNAL_ERROR, msg, None)
            }
            McpToolError::Transport(e) => ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("transport error: {e}"),
                None,
            ),
            McpToolError::Serialization(e) => ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("serialization error: {e}"),
                None,
            ),
        }
    }
}

/// Serde shape for the HTTP API error body.
/// `request_id` is present but we ALWAYS drop it (Metis directive).
#[derive(Debug, serde::Deserialize)]
struct ApiErrorBody {
    error: Option<String>,
    message: Option<String>,
    match_count: Option<u64>,
}

/// Inspect an HTTP response:
/// - 2xx → return it unchanged
/// - 416 → return `RangeNotSatisfiable` WITHOUT reading body (RFC 7233 §4.4)
/// - Other error → attempt JSON body parse; map from status code if parse fails
pub(crate) async fn map_response(
    resp: reqwest::Response,
) -> Result<reqwest::Response, McpToolError> {
    let status = resp.status();

    if status.is_success() {
        return Ok(resp);
    }

    // 416 has no body per RFC 7233 §4.4 — never attempt deserialization
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(McpToolError::RangeNotSatisfiable);
    }

    // Attempt body parse for all other errors
    let body_text = resp.text().await.unwrap_or_default();
    let parsed: Option<ApiErrorBody> = serde_json::from_str(&body_text).ok();

    let message = parsed
        .as_ref()
        .and_then(|b| b.message.as_deref())
        .map_or_else(|| format!("HTTP {}", status.as_u16()), str::to_owned);

    Err(match status.as_u16() {
        400 => McpToolError::InvalidRequest(message),
        401 => McpToolError::Unauthorized,
        404 => McpToolError::NotFound(message),
        412 => McpToolError::PreconditionFailed,
        413 => McpToolError::PayloadTooLarge,
        422 => match parsed.as_ref().and_then(|b| b.error.as_deref()) {
            Some("no_match") => McpToolError::NoMatch(message),
            Some("ambiguous_match") => {
                let count = parsed.as_ref().and_then(|b| b.match_count).unwrap_or(0);
                McpToolError::AmbiguousMatch { count }
            }
            _ => McpToolError::InvalidRequest(message),
        },
        503 => McpToolError::BackendUnavailable,
        _ => McpToolError::InternalError(format!("HTTP {}: {message}", status.as_u16())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    #[test]
    fn all_error_variants_convert_to_error_data() {
        // Ensure From<McpToolError> for ErrorData works without panics
        let cases: Vec<McpToolError> = vec![
            McpToolError::InvalidRequest("bad input".into()),
            McpToolError::Unauthorized,
            McpToolError::NotFound("kb missing".into()),
            McpToolError::PreconditionFailed,
            McpToolError::PayloadTooLarge,
            McpToolError::RangeNotSatisfiable,
            McpToolError::NoMatch("hello".into()),
            McpToolError::AmbiguousMatch { count: 3 },
            McpToolError::BackendUnavailable,
            McpToolError::InternalError("boom".into()),
        ];
        for e in cases {
            let _ed: ErrorData = e.into();
        }
    }

    #[test]
    fn no_match_display_and_ambiguous_display() {
        // Given: tool errors for replacement matching failures
        let no_match = McpToolError::NoMatch("hello".into());
        let ambiguous = McpToolError::AmbiguousMatch { count: 3 };

        // When: errors are rendered for MCP tool responses
        let no_match_display = no_match.to_string();
        let ambiguous_display = ambiguous.to_string();

        // Then: each variant exposes its distinct tool-level message
        assert_eq!(no_match_display, "no match found for old_string: hello");
        assert_eq!(
            ambiguous_display,
            "multiple matches (3); use replace_all to replace them all"
        );
    }

    #[tokio::test]
    async fn range_not_satisfiable_no_body_deserialization() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(416).insert_header("Content-Range", "bytes */1000"),
                // IMPORTANT: no body set — this proves we don't try to read one
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        let result = map_response(resp).await;
        assert!(
            matches!(result, Err(McpToolError::RangeNotSatisfiable)),
            "Expected RangeNotSatisfiable, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn request_id_dropped_from_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "kb x missing",
                "request_id": "REQ_ABC_SECRET"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        let result = map_response(resp).await;
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("kb x missing"),
            "message should be present: {err_str}"
        );
        assert!(
            !err_str.contains("REQ_ABC_SECRET"),
            "request_id MUST NOT appear in error: {err_str}"
        );
    }

    #[tokio::test]
    async fn all_status_codes_map_correctly() {
        type StatusCheck = fn(&McpToolError) -> bool;
        let test_cases: &[(u16, StatusCheck)] = &[
            (400, |e| matches!(e, McpToolError::InvalidRequest(_))),
            (401, |e| matches!(e, McpToolError::Unauthorized)),
            (404, |e| matches!(e, McpToolError::NotFound(_))),
            (412, |e| matches!(e, McpToolError::PreconditionFailed)),
            (413, |e| matches!(e, McpToolError::PayloadTooLarge)),
            (416, |e| matches!(e, McpToolError::RangeNotSatisfiable)),
            (500, |e| matches!(e, McpToolError::InternalError(_))),
            (503, |e| matches!(e, McpToolError::BackendUnavailable)),
        ];

        for (status, check_fn) in test_cases {
            let server = MockServer::start().await;

            let body = if *status == 416 {
                String::new()
            } else {
                serde_json::json!({
                    "error": "test_error",
                    "message": "test message"
                })
                .to_string()
            };

            Mock::given(method("GET"))
                .respond_with(if *status == 416 {
                    ResponseTemplate::new(*status).insert_header("Content-Range", "bytes */100")
                } else {
                    ResponseTemplate::new(*status).set_body_string(body)
                })
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{}/test", server.uri()))
                .send()
                .await
                .unwrap();

            let result = map_response(resp).await;
            assert!(result.is_err(), "status {status} should produce error");

            let err = result.unwrap_err();
            assert!(
                check_fn(&err),
                "status {status}: error variant mismatch: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn map_response_422_no_match_returns_no_match_variant() {
        // Given: a 422 API response with the no_match error code
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "no_match",
                "message": "needle"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        // When: the response is mapped into an MCP tool error
        let result = map_response(resp).await;

        // Then: the no_match API code maps to the NoMatch variant
        assert!(
            matches!(result, Err(McpToolError::NoMatch(ref message)) if message == "needle"),
            "Expected NoMatch, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn map_response_422_ambiguous_match_extracts_count() {
        // Given: a 422 API response with the ambiguous_match error code and count
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "ambiguous_match",
                "message": "too many matches",
                "match_count": 3
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        // When: the response is mapped into an MCP tool error
        let result = map_response(resp).await;

        // Then: the ambiguous_match API code maps to AmbiguousMatch with count
        assert!(
            matches!(result, Err(McpToolError::AmbiguousMatch { count: 3 })),
            "Expected AmbiguousMatch count 3, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn map_response_422_ambiguous_match_missing_count_defaults_to_zero() {
        // Given: a 422 ambiguous_match API response without match_count
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "ambiguous_match",
                "message": "too many matches"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        // When: the response is mapped into an MCP tool error
        let result = map_response(resp).await;

        // Then: a missing match_count defaults to zero
        assert!(
            matches!(result, Err(McpToolError::AmbiguousMatch { count: 0 })),
            "Expected AmbiguousMatch count 0, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn map_response_422_unknown_error_code_falls_back_to_invalid_request() {
        // Given: a 422 API response with an unknown error code
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "weird_code",
                "message": "?"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        // When: the response is mapped into an MCP tool error
        let result = map_response(resp).await;

        // Then: unrecognized 422 codes preserve the existing invalid request behavior
        assert!(
            matches!(result, Err(McpToolError::InvalidRequest(ref message)) if message == "?"),
            "Expected InvalidRequest, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn map_response_412_body_shape_unchanged_after_apierrorbody_extension() {
        // Given: a 412 API response using the existing error/message body shape
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error": "precondition_failed",
                "message": "stale etag"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/test", server.uri()))
            .send()
            .await
            .unwrap();

        // When: the response is mapped into an MCP tool error
        let result = map_response(resp).await;

        // Then: 412 still maps to PreconditionFailed
        assert!(
            matches!(result, Err(McpToolError::PreconditionFailed)),
            "Expected PreconditionFailed, got: {result:?}"
        );
    }
}
