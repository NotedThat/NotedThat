//! HTTP API error types and JSON envelope.

use axum::Json;
use axum::http::StatusCode;
use axum::http::header::CONTENT_RANGE;
use axum::response::{IntoResponse, Response};
use notedthat_core::{Error as CoreError, StorageError};
use serde::Serialize;

/// HTTP-layer error — maps domain and storage errors to HTTP status codes.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request lacked valid Bearer credentials.
    #[error("unauthorized")]
    Unauthorized,
    /// A domain error from `notedthat-core`.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// A storage-layer error not otherwise promoted to a top-level variant.
    ///
    /// Note: `StorageError::NotModified`, `StorageError::PreconditionFailed`, and
    /// `StorageError::RangeNotSatisfiable` are promoted via the manual `From<StorageError>`
    /// impl to top-level `ApiError` variants so that `IntoResponse` can emit the
    /// RFC-required headers (e.g. `Content-Range: bytes */N` for 416).
    #[error(transparent)]
    Storage(StorageError),
    /// A conditional PUT/DELETE failed because a precondition was not met.
    ///
    /// Maps to HTTP 412 Precondition Failed.
    #[error("precondition failed")]
    PreconditionFailed,
    /// The requested byte range could not be satisfied.
    ///
    /// `complete_length` is the total object size in bytes.  The `IntoResponse`
    /// impl emits `Content-Range: bytes */complete_length` per RFC 7233 §4.4.
    #[error("range not satisfiable")]
    RangeNotSatisfiable {
        /// Total object size in bytes.
        complete_length: u64,
    },
    /// The backend returned 304 Not Modified for a conditional GET/HEAD.
    ///
    /// Maps to HTTP 304 with no body, as required by RFC 7232 §4.1.
    #[error("not modified")]
    NotModified,
    /// The `Range:` header value could not be parsed per RFC 7233.
    ///
    /// Maps to HTTP 400 Bad Request.
    #[error("malformed range: {0}")]
    MalformedRange(String),
}

/// Promote specific `StorageError` variants to top-level `ApiError` variants so
/// that `IntoResponse` can emit the RFC-mandated response headers.
impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotModified => Self::NotModified,
            StorageError::PreconditionFailed => Self::PreconditionFailed,
            StorageError::RangeNotSatisfiable { complete_length } => {
                Self::RangeNotSatisfiable { complete_length }
            }
            other => Self::Storage(other),
        }
    }
}

/// JSON error response body shape: `{ "error": "code", "message": "...", "request_id": "..." }`.
#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: String,
    request_id: String,
}

/// An [`ApiError`] paired with a `request_id` string so the JSON body and the
/// `x-request-id` response header both contain the same value.
pub struct ApiErrorResponse {
    /// The underlying error.
    pub error: ApiError,
    /// The request ID (from the `x-request-id` header via `tower-http`).
    pub request_id: String,
}

impl ApiErrorResponse {
    /// Build an unauthorized response with the provided request ID.
    #[must_use]
    pub fn unauthorized(request_id: String) -> Self {
        Self {
            error: ApiError::Unauthorized,
            request_id,
        }
    }
}

impl ApiError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Core(CoreError::InvalidInput { .. }) => {
                (StatusCode::BAD_REQUEST, "invalid_request")
            }
            Self::Core(CoreError::NotFound { .. }) => (StatusCode::NOT_FOUND, "not_found"),
            Self::Core(CoreError::PayloadTooLarge { .. }) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large")
            }
            Self::Core(CoreError::MalformedRange(_)) | Self::MalformedRange(_) => {
                (StatusCode::BAD_REQUEST, "malformed_range")
            }
            Self::Core(CoreError::NotModified) | Self::NotModified => {
                (StatusCode::NOT_MODIFIED, "not_modified")
            }
            Self::Core(CoreError::PreconditionFailed) | Self::PreconditionFailed => {
                (StatusCode::PRECONDITION_FAILED, "precondition_failed")
            }
            Self::Core(CoreError::RangeNotSatisfiable { .. })
            | Self::RangeNotSatisfiable { .. } => {
                (StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable")
            }
            Self::Core(CoreError::BucketNameTooLong { .. } | CoreError::Config { .. }) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
            Self::Core(CoreError::Storage(e)) | Self::Storage(e) => Self::storage_status(e),
        }
    }

    fn storage_status(e: &StorageError) -> (StatusCode, &'static str) {
        match e {
            StorageError::NotFound { .. } | StorageError::BucketNotFound { .. } => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            StorageError::BackendUnavailable { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, "backend_unavailable")
            }
            StorageError::NotModified => (StatusCode::NOT_MODIFIED, "not_modified"),
            StorageError::PreconditionFailed => {
                (StatusCode::PRECONDITION_FAILED, "precondition_failed")
            }
            StorageError::RangeNotSatisfiable { .. } => {
                (StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable")
            }
            StorageError::Other { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        }
    }

    /// Extract the `complete_length` for a 416 response, regardless of which
    /// wrapper the `RangeNotSatisfiable` error arrived in.
    fn range_not_satisfiable_length(&self) -> Option<u64> {
        match self {
            Self::RangeNotSatisfiable { complete_length }
            | Self::Storage(StorageError::RangeNotSatisfiable { complete_length })
            | Self::Core(CoreError::RangeNotSatisfiable { complete_length }) => {
                Some(*complete_length)
            }
            _ => None,
        }
    }

    /// Return `true` for all variants that map to HTTP 304 (empty body required).
    fn is_not_modified(&self) -> bool {
        matches!(
            self,
            Self::NotModified
                | Self::Storage(StorageError::NotModified)
                | Self::Core(CoreError::NotModified)
        )
    }
}

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> Response {
        // 416 Range Not Satisfiable: RFC 7233 §4.4 requires
        // `Content-Range: bytes */N` and an empty body.
        if let Some(complete_length) = self.error.range_not_satisfiable_length() {
            let content_range = format!("bytes */{complete_length}");
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(CONTENT_RANGE, content_range)],
            )
                .into_response();
        }

        // 304 Not Modified: RFC 7232 §4.1 forbids a message body.
        if self.error.is_not_modified() {
            return StatusCode::NOT_MODIFIED.into_response();
        }

        // All other variants return a JSON error body.
        let (status, code) = self.error.status_and_code();
        let message = self.error.to_string();
        let body = ErrorBody {
            error: code,
            message,
            request_id: self.request_id,
        };
        (status, Json(body)).into_response()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        ApiErrorResponse {
            error: self,
            request_id: "unknown".to_string(),
        }
        .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn test_unauthorized_status_and_body() {
        let resp = ApiErrorResponse::unauthorized("req-123".to_string()).into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "unauthorized");
        assert_eq!(json["request_id"], "req-123");
    }

    #[tokio::test]
    async fn test_not_found_status() {
        let err = ApiError::Core(CoreError::NotFound {
            resource: "foo".into(),
        });
        let resp = ApiErrorResponse {
            error: err,
            request_id: "rid".into(),
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_payload_too_large_status() {
        let err = ApiError::Core(CoreError::PayloadTooLarge {
            size: 20_000_000,
            limit: 16_777_216,
        });
        let resp = ApiErrorResponse {
            error: err,
            request_id: "rid".into(),
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_request_id_in_body() {
        let err = ApiError::Core(CoreError::InvalidInput {
            message: "bad".into(),
        });
        let resp = ApiErrorResponse {
            error: err,
            request_id: "my-req-id".into(),
        }
        .into_response();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["request_id"], "my-req-id");
    }

    // ─── New variant tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_precondition_failed_412() {
        let resp = ApiError::PreconditionFailed.into_response();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn test_not_modified_304_empty_body() {
        let resp = ApiError::NotModified.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "304 must have an empty body");
    }

    #[tokio::test]
    async fn test_malformed_range_400() {
        let resp = ApiError::MalformedRange("bytes=abc".to_string()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// RFC 7233 §4.4: a 416 response MUST include `Content-Range: bytes */N`.
    #[tokio::test]
    async fn test_range_not_satisfiable_416_content_range_header() {
        let resp = ApiError::RangeNotSatisfiable {
            complete_length: 100,
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let cr = resp
            .headers()
            .get("content-range")
            .expect("content-range header must be present on 416");
        assert_eq!(cr.to_str().unwrap(), "bytes */100");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "416 body must be empty per RFC 7233 §4.4");
    }

    /// Same 416 check via the `ApiError::Storage(...)` wrapper (e.g. from router.rs
    /// call sites that use explicit wrapping instead of `Into::into`).
    #[tokio::test]
    async fn test_storage_range_not_satisfiable_416_content_range_header() {
        let resp = ApiError::Storage(StorageError::RangeNotSatisfiable {
            complete_length: 42,
        })
        .into_response();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let cr = resp
            .headers()
            .get("content-range")
            .expect("content-range header must be present on 416");
        assert_eq!(cr.to_str().unwrap(), "bytes */42");
    }

    /// 304 from a wrapped `StorageError::NotModified` (explicit wrapping in router.rs).
    #[tokio::test]
    async fn test_storage_not_modified_304_empty_body() {
        let resp = ApiError::Storage(StorageError::NotModified).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "304 must have an empty body");
    }

    /// `From<StorageError>` promotes `NotModified` to `ApiError::NotModified`.
    #[test]
    fn test_from_storage_error_not_modified() {
        let api_err = ApiError::from(StorageError::NotModified);
        assert!(matches!(api_err, ApiError::NotModified));
    }

    /// `From<StorageError>` promotes `PreconditionFailed` to `ApiError::PreconditionFailed`.
    #[test]
    fn test_from_storage_error_precondition_failed() {
        let api_err = ApiError::from(StorageError::PreconditionFailed);
        assert!(matches!(api_err, ApiError::PreconditionFailed));
    }

    /// `From<StorageError>` promotes `RangeNotSatisfiable` with the correct length.
    #[test]
    fn test_from_storage_error_range_not_satisfiable() {
        let api_err = ApiError::from(StorageError::RangeNotSatisfiable {
            complete_length: 999,
        });
        assert!(
            matches!(
                api_err,
                ApiError::RangeNotSatisfiable {
                    complete_length: 999
                }
            ),
            "expected RangeNotSatisfiable with complete_length=999, got {api_err:?}"
        );
    }

    /// Other `StorageError` variants must still be wrapped in `ApiError::Storage`.
    #[test]
    fn test_from_storage_error_other_wrapped() {
        let api_err = ApiError::from(StorageError::NotFound {
            key: "foo".to_string(),
        });
        assert!(matches!(api_err, ApiError::Storage(_)));
    }
}
