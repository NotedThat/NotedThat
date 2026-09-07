//! HTTP 503 response builders for indexer backpressure on `WebDAV` write paths.
//!
//! Bodies mirror the HTTP API `backend_unavailable` shape so `WebDAV` clients
//! see the same semantics as HTTP clients. Each variant carries an
//! operation-specific message describing the exact partial-completion state.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(super) fn dav_error_body(condition: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:" xmlns:nt="urn:notedthat:error"><nt:{condition}/></D:error>"#
    )
}

/// HTTP 503 response for `WriteError::IndexerBackpressureUpsert`.
pub(super) fn backpressure_response() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"object stored; indexer queue full — retry to re-enqueue"}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

/// HTTP 503 response for `WriteError::IndexerBackpressureTombstone` (DELETE).
pub(super) fn delete_backpressure_response() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"deleted from storage; retry to clear from search index"}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) fn move_destination_backpressure_response(_req_id: &str) -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"destination write succeeded but destination index event failed; source unchanged. Retry MOVE to re-enqueue destination index event."}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) fn copy_destination_backpressure_response() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"destination write succeeded but destination index event failed. Retry COPY to re-enqueue destination index event."}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) fn move_source_tombstone_backpressure_response(_req_id: &str) -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"destination write succeeded and source deleted from storage, but source search-index tombstone failed — search may return stale entries for the source path until retry or reindex. Retry MOVE to re-enqueue the source tombstone; the destination write is idempotent."}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}
