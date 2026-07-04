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
    #[allow(dead_code)]
    error: Option<String>,
    message: Option<String>,
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
        .and_then(|b| b.message)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));

    Err(match status.as_u16() {
        400 => McpToolError::InvalidRequest(message),
        401 => McpToolError::Unauthorized,
        404 => McpToolError::NotFound(message),
        412 => McpToolError::PreconditionFailed,
        413 => McpToolError::PayloadTooLarge,
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
            McpToolError::BackendUnavailable,
            McpToolError::InternalError("boom".into()),
        ];
        for e in cases {
            let _ed: ErrorData = e.into();
        }
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
}
