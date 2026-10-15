//! XML response bodies the middleware writes itself, rather than dav-server.

use axum::http::header::CONTENT_TYPE as CONTENT_TYPE_HEADER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// Content type of every XML body (RFC 4918 §8.2).
pub(crate) const CONTENT_TYPE: &str = "application/xml; charset=utf-8";

/// A `DAV:error` response (RFC 4918 §16) naming the `NotedThat` precondition
/// `condition` in the `urn:notedthat:error` namespace.
pub(crate) fn error(status: StatusCode, condition: &str) -> Response {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:" xmlns:nt="urn:notedthat:error"><nt:{condition}/></D:error>"#
    );
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE_HEADER, HeaderValue::from_static(CONTENT_TYPE));
    response
}
