//! HTTP 503 response builders for indexer backpressure on `WebDAV` write paths.
//!
//! Bodies mirror the HTTP API `backend_unavailable` shape so `WebDAV` clients
//! see the same semantics as HTTP clients. Each variant carries an
//! operation-specific message describing the exact partial-completion state.

use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// HTTP 503 `backend_unavailable` JSON response carrying `message`, asking the
/// client to retry after 5 seconds.
fn unavailable(message: &str) -> Response {
    // The messages are literals without quotes or control characters, so no
    // escaping is needed to keep the body valid JSON.
    let body = format!(r#"{{"error":"backend_unavailable","message":"{message}"}}"#);
    let mut resp = (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
    resp.headers_mut().insert(
        HeaderName::from_static("retry-after"),
        HeaderValue::from_static("5"),
    );
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// HTTP 503 response for `WriteError::IndexerBackpressureUpsert`.
pub(super) fn backpressure_response() -> Response {
    unavailable("object stored; indexer queue full — retry to re-enqueue")
}

/// HTTP 503 response for `WriteError::IndexerBackpressureTombstone` (DELETE).
pub(super) fn delete_backpressure_response() -> Response {
    unavailable("deleted from storage; retry to clear from search index")
}

pub(super) fn move_destination_backpressure_response() -> Response {
    unavailable(
        "destination write succeeded but destination index event failed; source unchanged. Retry MOVE to re-enqueue destination index event.",
    )
}

/// HTTP 503 response for `WriteError::EventPublishFailed`: storage took the
/// write, the event log did not, and the operation is idempotent to retry.
pub(super) fn event_publish_failed_response(message: &str) -> Response {
    unavailable(message)
}

pub(super) fn copy_destination_backpressure_response() -> Response {
    unavailable(
        "destination write succeeded but destination index event failed. Retry COPY to re-enqueue destination index event.",
    )
}

pub(super) fn move_source_tombstone_backpressure_response() -> Response {
    unavailable(
        "destination write succeeded and source deleted from storage, but source search-index tombstone failed — search may return stale entries for the source path until retry or reindex. Send DELETE for the source to re-enqueue its tombstone — DELETE of a missing key is idempotent and still enqueues, whereas a retried MOVE would find no source.",
    )
}
