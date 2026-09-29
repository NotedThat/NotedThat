//! The validator headers every object response carries.
//!
//! A 200, a 206 and a 304 for the same object must agree on `ETag` and
//! `Last-Modified` (RFC 9110 §15.4.5), so all of them add the headers here.

use axum::http::header::{ETAG, LAST_MODIFIED};
use axum::http::response::Builder;
use std::time::{Duration, UNIX_EPOCH};

/// Add `ETag` and `Last-Modified` to `builder`, skipping whichever is unknown.
///
/// `last_modified` is Unix seconds, as [`notedthat_core::ObjectMeta`] carries it.
pub(crate) fn with_validators(
    mut builder: Builder,
    etag: Option<&str>,
    last_modified: Option<i64>,
) -> Builder {
    if let Some(etag) = etag {
        builder = builder.header(ETAG, etag);
    }
    if let Some(last_modified) = last_modified
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
    {
        builder = builder.header(LAST_MODIFIED, httpdate::fmt_http_date(last_modified));
    }
    builder
}
