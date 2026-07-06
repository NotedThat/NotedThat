//! HTTP API error types and JSON envelope.

use axum::Json;
use axum::http::header::{CONTENT_RANGE, RETRY_AFTER};
use axum::http::{HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use notedthat_core::{Error as CoreError, StorageError};
use serde::Serialize;

/// HTTP-layer error — maps domain and storage errors to HTTP status codes.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request lacked valid Bearer credentials.
    #[error("unauthorized")]
    Unauthorized,
    /// Indexer queue was full while enqueueing an upsert (PUT/COPY).
    #[error("indexer upsert backpressure")]
    IndexerBackpressureUpsert,
    /// Indexer queue was full while enqueueing a tombstone (DELETE).
    #[error("indexer tombstone backpressure")]
    IndexerBackpressureTombstone,
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
    /// A `Range: lines=…` header specified a range that exceeds the object's line count.
    ///
    /// Maps to HTTP 416 with `Content-Range: lines */<line_total>` and
    /// `X-Content-Range-Bytes: */<byte_total>` per the line-range extension.
    #[error("line range not satisfiable")]
    LineRangeNotSatisfiable {
        /// Total line count in the object.
        line_total: u64,
        /// Total byte count in the object.
        byte_total: u64,
    },
    /// A single-replace operation found no occurrence of `old_string`.
    #[error("no match found for old_string")]
    ReplaceNoMatch,
    /// A single-replace operation found multiple occurrences of `old_string`.
    #[error("multiple matches found ({count}); use replace_all to replace them all")]
    ReplaceAmbiguous {
        /// Number of occurrences found.
        count: u64,
    },
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

impl From<notedthat_write::WriteError> for ApiError {
    fn from(e: notedthat_write::WriteError) -> Self {
        match e {
            notedthat_write::WriteError::Storage(e) => Self::Storage(e),
            notedthat_write::WriteError::TooLarge { size, limit }
            | notedthat_write::WriteError::PatchTooLarge { size, limit } => {
                Self::Core(CoreError::PayloadTooLarge { size, limit })
            }
            notedthat_write::WriteError::Path(e) => Self::Core(e),
            notedthat_write::WriteError::IndexerBackpressureUpsert => {
                Self::IndexerBackpressureUpsert
            }
            notedthat_write::WriteError::IndexerBackpressureTombstone => {
                Self::IndexerBackpressureTombstone
            }
            notedthat_write::WriteError::PatchLineOutOfRange {
                total_lines,
                total_bytes,
                ..
            } => Self::LineRangeNotSatisfiable {
                line_total: total_lines,
                byte_total: total_bytes,
            },
            notedthat_write::WriteError::PatchInvalidRange { message } => {
                Self::Core(CoreError::InvalidInput { message })
            }
            notedthat_write::WriteError::ReplaceNoMatch => Self::ReplaceNoMatch,
            notedthat_write::WriteError::ReplaceAmbiguous { count } => {
                Self::ReplaceAmbiguous { count }
            }
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

#[derive(Serialize)]
struct ReplaceAmbiguousBody<'a> {
    error: &'a str,
    message: String,
    request_id: String,
    match_count: u64,
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
            Self::IndexerBackpressureUpsert | Self::IndexerBackpressureTombstone => {
                (StatusCode::SERVICE_UNAVAILABLE, "backend_unavailable")
            }
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
            Self::LineRangeNotSatisfiable { .. }
            | Self::Core(CoreError::RangeNotSatisfiable { .. })
            | Self::RangeNotSatisfiable { .. } => {
                (StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable")
            }
            Self::Core(CoreError::NotModified) | Self::NotModified => {
                (StatusCode::NOT_MODIFIED, "not_modified")
            }
            Self::Core(CoreError::PreconditionFailed) | Self::PreconditionFailed => {
                (StatusCode::PRECONDITION_FAILED, "precondition_failed")
            }
            Self::ReplaceNoMatch => (StatusCode::UNPROCESSABLE_ENTITY, "no_match"),
            Self::ReplaceAmbiguous { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "ambiguous_match"),
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
        // Line-mode 416: emit Content-Range: lines */<total> + X-Content-Range-Bytes: */<total_bytes>.
        if let ApiError::LineRangeNotSatisfiable {
            line_total,
            byte_total,
        } = &self.error
        {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [
                    (CONTENT_RANGE, format!("lines */{line_total}")),
                    (
                        HeaderName::from_static("x-content-range-bytes"),
                        format!("*/{byte_total}"),
                    ),
                ],
            )
                .into_response();
        }

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

        match &self.error {
            ApiError::IndexerBackpressureUpsert => {
                let body = ErrorBody {
                    error: "backend_unavailable",
                    message: "object stored; indexer queue full — retry to re-enqueue".to_string(),
                    request_id: self.request_id,
                };
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(RETRY_AFTER, "5")],
                    Json(body),
                )
                    .into_response();
            }
            ApiError::IndexerBackpressureTombstone => {
                let body = ErrorBody {
                    error: "backend_unavailable",
                    message: "deleted from storage; retry to clear from search index".to_string(),
                    request_id: self.request_id,
                };
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(RETRY_AFTER, "5")],
                    Json(body),
                )
                    .into_response();
            }
            ApiError::ReplaceAmbiguous { count } => {
                let body = ReplaceAmbiguousBody {
                    error: "ambiguous_match",
                    message: self.error.to_string(),
                    request_id: self.request_id,
                    match_count: *count,
                };
                return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
            }
            _ => {}
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
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use bytes::Bytes;
    use notedthat_core::KbSlug;
    use notedthat_write::WriteError;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    const KB: &str = "notes";
    const TOKEN: &str = "test-token-abc";

    fn router() -> axum::Router {
        let kb = KbSlug::try_new(KB).unwrap();
        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb);
        let (indexer_tx, mut rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        crate::router::build_router(crate::state::AppState {
            storage: Arc::new(crate::testing::InMemoryStorage::default()),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: 16 * 1024 * 1024,
            max_patchable_size: 16 * 1024 * 1024,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        })
    }

    async fn assert_invalid_request_response(response: Response) {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid_request");
    }

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

    #[tokio::test]
    async fn test_indexer_backpressure_upsert_503_body_and_retry_after() {
        let resp = ApiErrorResponse {
            error: ApiError::IndexerBackpressureUpsert,
            request_id: "rid".to_string(),
        }
        .into_response();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "backend_unavailable");
        assert_eq!(
            json["message"],
            "object stored; indexer queue full — retry to re-enqueue"
        );
        assert_eq!(json["request_id"], "rid");
    }

    #[tokio::test]
    async fn test_indexer_backpressure_tombstone_503_body_and_retry_after() {
        let resp = ApiErrorResponse {
            error: ApiError::IndexerBackpressureTombstone,
            request_id: "rid".to_string(),
        }
        .into_response();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "backend_unavailable");
        assert_eq!(
            json["message"],
            "deleted from storage; retry to clear from search index"
        );
        assert_eq!(json["request_id"], "rid");
    }

    #[test]
    fn test_from_write_error_indexer_backpressure() {
        assert!(matches!(
            ApiError::from(WriteError::IndexerBackpressureUpsert),
            ApiError::IndexerBackpressureUpsert
        ));
        assert!(matches!(
            ApiError::from(WriteError::IndexerBackpressureTombstone),
            ApiError::IndexerBackpressureTombstone
        ));
    }

    #[test]
    fn test_from_write_error_patch_too_large() {
        let api_err = ApiError::from(WriteError::PatchTooLarge {
            size: 200 * 1024 * 1024,
            limit: 100 * 1024 * 1024,
        });

        let (status, code) = api_err.status_and_code();
        assert_eq!(status.as_u16(), 413);
        assert_eq!(code, "payload_too_large");
    }

    #[test]
    fn test_from_write_error_patch_line_out_of_range() {
        let api_err = ApiError::from(WriteError::PatchLineOutOfRange {
            first: 999,
            last: 1000,
            total_lines: 20,
            total_bytes: 100,
        });

        let (status, code) = api_err.status_and_code();
        assert_eq!(status.as_u16(), 416);
        assert_eq!(code, "range_not_satisfiable");
    }

    #[test]
    fn test_from_write_error_patch_invalid_range() {
        let api_err = ApiError::from(WriteError::PatchInvalidRange {
            message: "test".into(),
        });

        let (status, code) = api_err.status_and_code();
        assert_eq!(status.as_u16(), 400);
        assert_eq!(code, "invalid_request");
    }

    #[test]
    fn test_from_write_error_replace_no_match() {
        let api_err = ApiError::from(WriteError::ReplaceNoMatch);

        let (status, code) = api_err.status_and_code();
        assert_eq!(status.as_u16(), 422);
        assert_eq!(code, "no_match");
    }

    #[test]
    fn test_from_write_error_replace_ambiguous() {
        let api_err = ApiError::from(WriteError::ReplaceAmbiguous { count: 3 });

        let (status, code) = api_err.status_and_code();
        assert_eq!(status.as_u16(), 422);
        assert_eq!(code, "ambiguous_match");
    }

    #[tokio::test]
    async fn test_ambiguous_match_body_includes_match_count() {
        let resp = ApiErrorResponse {
            error: ApiError::ReplaceAmbiguous { count: 3 },
            request_id: "req-1".into(),
        }
        .into_response();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "ambiguous_match");
        assert_eq!(json["match_count"], 3);
        assert_eq!(json["request_id"], "req-1");
    }

    #[tokio::test]
    async fn test_no_match_body_omits_match_count() {
        let resp = ApiErrorResponse {
            error: ApiError::ReplaceNoMatch,
            request_id: "req-1".into(),
        }
        .into_response();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "no_match");
        assert!(json.get("match_count").is_none());
    }

    #[tokio::test]
    async fn replace_missing_if_match_returns_400_invalid_request() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/replace/hello.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(Bytes::from_static(
                        br#"{"old_string":"x","new_string":"y"}"#,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_invalid_request_response(response).await;
    }

    #[tokio::test]
    async fn replace_if_match_star_returns_400() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/replace/hello.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::IF_MATCH, "*")
                    .body(Body::from(Bytes::from_static(
                        br#"{"old_string":"x","new_string":"y"}"#,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_invalid_request_response(response).await;
    }

    #[tokio::test]
    async fn replace_multi_value_if_match_returns_400() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/replace/hello.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::IF_MATCH, "\"a\",\"b\"")
                    .body(Body::from(Bytes::from_static(
                        br#"{"old_string":"x","new_string":"y"}"#,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_invalid_request_response(response).await;
    }

    #[tokio::test]
    async fn replace_malformed_json_body_returns_400() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/replace/hello.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::IF_MATCH, "\"valid-etag\"")
                    .body(Body::from(Bytes::from_static(b"{invalid json")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_invalid_request_response(response).await;
    }

    #[tokio::test]
    async fn replace_missing_old_string_field_returns_400() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/replace/hello.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::IF_MATCH, "\"valid-etag\"")
                    .body(Body::from(Bytes::from_static(br#"{"new_string":"y"}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_invalid_request_response(response).await;
    }

    #[tokio::test]
    async fn replace_empty_old_string_returns_400() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/replace/hello.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::IF_MATCH, "\"valid-etag\"")
                    .body(Body::from(Bytes::from_static(
                        br#"{"old_string":"","new_string":"y"}"#,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_invalid_request_response(response).await;
    }

    #[tokio::test]
    async fn test_precondition_failed_body_shape_unchanged() {
        let resp = ApiErrorResponse {
            error: ApiError::PreconditionFailed,
            request_id: "req-1".into(),
        }
        .into_response();

        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object.len(), 3);
        assert!(object.contains_key("error"));
        assert!(object.contains_key("message"));
        assert!(object.contains_key("request_id"));
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

    mod line_range_error {
        use super::*;
        use axum::body::Body;
        use axum::http::Request;
        use bytes::Bytes;
        use notedthat_core::KbSlug;
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use tower::util::ServiceExt;

        const KB: &str = "notes";
        const TOKEN: &str = "test-token-abc";

        fn twenty_line_markdown() -> String {
            let mut body = String::new();
            for line in 1..=20 {
                std::fmt::Write::write_fmt(&mut body, format_args!("line {line:02}\n")).unwrap();
            }
            body
        }

        fn router() -> axum::Router {
            let kb = KbSlug::try_new(KB).unwrap();
            let mut kbs = BTreeMap::new();
            kbs.insert(KB.to_string(), kb);
            let (indexer_tx, mut rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });

            crate::router::build_router(crate::state::AppState {
                storage: Arc::new(crate::testing::InMemoryStorage::default()),
                declared_kbs: Arc::new(kbs),
                bearer_token: Arc::new(TOKEN.to_string()),
                max_body_size: 16 * 1024 * 1024,
                max_patchable_size: 16 * 1024 * 1024,
                indexer_tx,
                searcher: Arc::new(crate::testing::NoopSearcher),
            })
        }

        async fn put_ranges_md(router: axum::Router) {
            let response = router
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!("/v1/knowledgebases/{KB}/ranges.md"))
                        .header("authorization", format!("Bearer {TOKEN}"))
                        .header(axum::http::header::CONTENT_TYPE, "text/markdown")
                        .body(Body::from(Bytes::from(twenty_line_markdown())))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::CREATED);
        }

        #[tokio::test]
        async fn malformed_line_range_returns_json_400() {
            let response = ApiError::MalformedRange("lines=abc".into()).into_response();

            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "malformed_range");
        }

        #[tokio::test]
        async fn line_range_not_satisfiable_returns_dual_headers_and_empty_body() {
            let response = ApiError::LineRangeNotSatisfiable {
                line_total: 20,
                byte_total: 100,
            }
            .into_response();

            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                response.headers().get("content-range").unwrap(),
                "lines */20"
            );
            assert_eq!(
                response.headers().get("x-content-range-bytes").unwrap(),
                "*/100"
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(body.is_empty());
        }

        #[tokio::test]
        async fn out_of_range_line_get_returns_dual_headers_and_empty_body() {
            let router = router();
            put_ranges_md(router.clone()).await;

            let response = router
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/v1/knowledgebases/{KB}/ranges.md"))
                        .header("authorization", format!("Bearer {TOKEN}"))
                        .header(axum::http::header::RANGE, "lines=100-200")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                response.headers().get("content-range").unwrap(),
                "lines */20"
            );
            assert_eq!(
                response.headers().get("x-content-range-bytes").unwrap(),
                "*/160"
            );
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(body.is_empty());
        }

        #[tokio::test]
        async fn byte_range_not_satisfiable_omits_line_byte_header() {
            let response = ApiError::RangeNotSatisfiable {
                complete_length: 100,
            }
            .into_response();

            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                response.headers().get("content-range").unwrap(),
                "bytes */100"
            );
            assert!(response.headers().get("x-content-range-bytes").is_none());
        }

        #[test]
        fn patch_line_out_of_range_maps_to_line_range_not_satisfiable() {
            let error = ApiError::from(WriteError::PatchLineOutOfRange {
                first: 100,
                last: 200,
                total_lines: 20,
                total_bytes: 100,
            });

            assert!(matches!(
                error,
                ApiError::LineRangeNotSatisfiable {
                    line_total: 20,
                    byte_total: 100
                }
            ));
        }
    }
}
