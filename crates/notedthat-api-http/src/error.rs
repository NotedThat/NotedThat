//! HTTP API error types and JSON envelope.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
    /// A storage-layer error.
    #[error(transparent)]
    Storage(#[from] StorageError),
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
        Self { error: ApiError::Unauthorized, request_id }
    }
}

impl ApiError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Core(CoreError::InvalidInput { .. }) => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Core(CoreError::NotFound { .. }) => (StatusCode::NOT_FOUND, "not_found"),
            Self::Core(CoreError::PayloadTooLarge { .. }) => {
                (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large")
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
            StorageError::Other { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        }
    }
}

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> Response {
        let (status, code) = self.error.status_and_code();
        let message = self.error.to_string();
        let body = ErrorBody { error: code, message, request_id: self.request_id };
        (status, Json(body)).into_response()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        ApiErrorResponse { error: self, request_id: "unknown".to_string() }.into_response()
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
        let err = ApiError::Core(CoreError::NotFound { resource: "foo".into() });
        let resp = ApiErrorResponse { error: err, request_id: "rid".into() }.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_payload_too_large_status() {
        let err = ApiError::Core(CoreError::PayloadTooLarge { size: 20_000_000, limit: 16_777_216 });
        let resp = ApiErrorResponse { error: err, request_id: "rid".into() }.into_response();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_request_id_in_body() {
        let err = ApiError::Core(CoreError::InvalidInput { message: "bad".into() });
        let resp = ApiErrorResponse { error: err, request_id: "my-req-id".into() }.into_response();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["request_id"], "my-req-id");
    }
}
