//! One status table for the `WebDAV` write path (§6.12, D43).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedthat_core::{ErrorCode, StorageError};
use notedthat_write::WriteError;

pub(super) fn dav_error_body(condition: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:" xmlns:nt="urn:notedthat:error"><nt:{condition}/></D:error>"#
    )
}

/// What to tell the client when storage did the work but a queue behind it did not.
#[derive(Clone, Copy)]
pub(super) struct Retry {
    pub queue_full: &'static str,
    pub publish_failed: &'static str,
}

/// 503 with `Retry-After: 5`, the HTTP API's `backend_unavailable` shape.
/// `message` is a literal without quotes or control characters.
pub(super) fn retry_later(message: &str) -> Response {
    let body = format!(
        r#"{{"error":"{}","message":"{message}"}}"#,
        ErrorCode::BackendUnavailable.as_str()
    );
    let mut resp = (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
    resp.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

/// The status of a storage failure. No `WebDAV` caller sends conditionals to
/// storage, so 304 and 416 cannot happen here and count as internal errors.
pub(super) fn storage_status(error: &StorageError) -> StatusCode {
    match error {
        StorageError::NotFound { .. } | StorageError::BucketNotFound { .. } => {
            StatusCode::NOT_FOUND
        }
        StorageError::BackendUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        StorageError::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        StorageError::NotModified(_)
        | StorageError::RangeNotSatisfiable { .. }
        | StorageError::Other { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The response for a failed write. The backend's text goes to the log only (D43).
pub(super) fn write_error_response(error: WriteError, retry: Retry) -> Response {
    match error {
        WriteError::TooLarge { .. } | WriteError::PatchTooLarge { .. } => {
            StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
        WriteError::Storage(error) => {
            let status = storage_status(&error);
            if status == StatusCode::INTERNAL_SERVER_ERROR {
                tracing::error!(error = %error, "WRITE_FAILED");
            }
            status.into_response()
        }
        WriteError::IndexerBackpressureUpsert | WriteError::IndexerBackpressureTombstone => {
            retry_later(retry.queue_full)
        }
        WriteError::EventPublishFailed { .. } => retry_later(retry.publish_failed),
        WriteError::InvalidManifest { message } | WriteError::PatchInvalidRange { message } => {
            (StatusCode::BAD_REQUEST, message).into_response()
        }
        WriteError::PreconditionRequired { .. } => {
            StatusCode::PRECONDITION_REQUIRED.into_response()
        }
        WriteError::PatchLineOutOfRange { .. } => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
        WriteError::ReplaceNoMatch | WriteError::ReplaceAmbiguous { .. } => {
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::Validators;
    use notedthat_write::WriteEffect;

    const RETRY: Retry = Retry {
        queue_full: "queue full",
        publish_failed: "publish failed",
    };

    fn unavailable() -> StorageError {
        StorageError::BackendUnavailable {
            message: "down".into(),
        }
    }

    #[test]
    fn every_storage_error_has_a_status() {
        let cases = [
            (
                StorageError::NotFound { key: "k".into() },
                StatusCode::NOT_FOUND,
            ),
            (
                StorageError::BucketNotFound { bucket: "b".into() },
                StatusCode::NOT_FOUND,
            ),
            (unavailable(), StatusCode::SERVICE_UNAVAILABLE),
            (
                StorageError::PreconditionFailed,
                StatusCode::PRECONDITION_FAILED,
            ),
            (
                StorageError::NotModified(Validators::default()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::RangeNotSatisfiable { complete_length: 1 },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::Other {
                    source: Box::new(std::io::Error::other("boom")),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(storage_status(&error), status, "{error}");
        }
    }

    #[test]
    fn every_write_error_has_a_status() {
        let cases = [
            (
                WriteError::TooLarge { size: 2, limit: 1 },
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                WriteError::PatchTooLarge { size: 2, limit: 1 },
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                WriteError::Storage(unavailable()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                WriteError::Storage(StorageError::BucketNotFound { bucket: "b".into() }),
                StatusCode::NOT_FOUND,
            ),
            (
                WriteError::Storage(StorageError::PreconditionFailed),
                StatusCode::PRECONDITION_FAILED,
            ),
            (
                WriteError::IndexerBackpressureUpsert,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                WriteError::IndexerBackpressureTombstone,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                WriteError::EventPublishFailed {
                    after: WriteEffect::Stored,
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                WriteError::InvalidManifest {
                    message: "m".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                WriteError::PatchInvalidRange {
                    message: "r".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                WriteError::PreconditionRequired {
                    message: "p".into(),
                },
                StatusCode::PRECONDITION_REQUIRED,
            ),
            (
                WriteError::PatchLineOutOfRange {
                    first: 1,
                    last: 2,
                    total_lines: 1,
                    total_bytes: 1,
                },
                StatusCode::RANGE_NOT_SATISFIABLE,
            ),
            (WriteError::ReplaceNoMatch, StatusCode::UNPROCESSABLE_ENTITY),
            (
                WriteError::ReplaceAmbiguous { count: 2 },
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];
        for (error, status) in cases {
            let name = error.to_string();
            let response = write_error_response(error, RETRY);
            assert_eq!(response.status(), status, "{name}");
            assert_eq!(
                response.headers().contains_key("retry-after"),
                status == StatusCode::SERVICE_UNAVAILABLE && !name.contains("down"),
                "{name}"
            );
        }
    }
}
