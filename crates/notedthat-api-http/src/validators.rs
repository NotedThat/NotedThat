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
    // An unformattable date is left out, as an unknown one is: the response
    // still answers, and a client simply has no date to revalidate against.
    if let Some(date) = last_modified.and_then(http_date) {
        builder = builder.header(LAST_MODIFIED, date);
    }
    builder
}

/// 9999-12-31T23:59:59Z in Unix seconds — the last instant `httpdate` will format.
pub const LATEST_FORMATTABLE: u64 = 253_402_300_799;

/// The RFC 7231 IMF-fixdate for `unix_seconds`, or `None` when it has none.
///
/// `httpdate::fmt_http_date` **panics** past year 9999, and `last_modified` is
/// whatever the backend put there — an `fs` mtime can be set to anything a
/// filesystem stores. A response or a page render must not panic on absurd
/// metadata, so the value is bounded here rather than trusted.
pub(crate) fn http_date(unix_seconds: i64) -> Option<String> {
    let seconds = u64::try_from(unix_seconds).ok()?;
    if seconds > LATEST_FORMATTABLE {
        return None;
    }
    let instant = UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    Some(httpdate::fmt_http_date(instant))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Response;

    #[test]
    fn a_last_modified_past_year_9999_is_left_out_rather_than_panicking() {
        for seconds in [300_000_000_000, i64::MAX] {
            // When
            let response = with_validators(Response::builder(), None, Some(seconds))
                .body(())
                .unwrap();

            // Then
            assert!(response.headers().get(LAST_MODIFIED).is_none(), "{seconds}");
        }
    }
}
