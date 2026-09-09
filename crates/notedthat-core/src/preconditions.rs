//! RFC 7232 conditional-request evaluation, for backends that must do it themselves.
//!
//! Per D9 `NotedThat` forwards [`ConditionalHeaders`] verbatim and lets the backend
//! decide. That works because S3 *is* a server: it compares the validators and answers
//! 304 or 412 on its own. `notedthat-storage-s3` is a pure forwarder and never calls
//! anything in this module.
//!
//! A backend with no server behind it — the filesystem adapter, and the in-memory
//! substitute — has to evaluate the conditions itself. Both do it from here rather than
//! each carrying its own copy, because two implementations of `If-None-Match` list
//! parsing is exactly how a substitute drifts away from the thing it stands in for.
//!
//! Comparison strength follows RFC 7232 §3: `If-Match` uses the strong comparison
//! function and `If-None-Match` the weak one, which is why only the latter strips the
//! `W/` prefix.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::conditional::ConditionalHeaders;
use crate::error::StorageError;
use crate::range::ByteRange;

/// The two facts about a stored object that every precondition check needs.
#[derive(Debug, Clone, Copy)]
pub struct ObjectState<'a> {
    /// The object's current `ETag`, quoted per RFC 7232 §2.3.
    pub etag: &'a str,
    /// The object's last modification time.
    pub last_modified: SystemTime,
}

/// Whole seconds since the Unix epoch, saturating at zero for pre-epoch times.
///
/// HTTP-dates carry one-second resolution, so every comparison in this module truncates
/// to seconds. A backend whose timestamps are finer must truncate the same way or a
/// sub-second remainder turns an `If-Modified-Since` equality into a `>`.
#[must_use]
pub fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_secs())
}

/// [`unix_seconds`] as the `i64` that [`crate::kb::ObjectMeta::last_modified`] carries.
#[must_use]
pub fn unix_seconds_i64(time: SystemTime) -> i64 {
    i64::try_from(unix_seconds(time)).unwrap_or(i64::MAX)
}

/// Parse an HTTP-date header value, or fail with the error the backends agree on.
///
/// Callers evaluate this *before* touching storage, so a malformed date on a missing
/// object reports the malformed date rather than the missing object.
pub fn parse_http_date_or_err(value: &str) -> Result<SystemTime, StorageError> {
    httpdate::parse_http_date(value).map_err(|error| StorageError::Other {
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid HTTP-date '{value}': {error}"),
        )),
    })
}

/// Strong `ETag` comparison, per RFC 7232 §3.1 (`If-Match`).
///
/// `*` matches any existing object. A comma-separated list matches if any member is
/// byte-equal to the current validator. Weak tags are deliberately not unwrapped: a weak
/// validator can never satisfy `If-Match`.
#[must_use]
pub fn matches_if_match(current_etag: &str, if_match_value: &str) -> bool {
    if if_match_value.trim() == "*" {
        return true;
    }

    if_match_value
        .split(',')
        .map(str::trim)
        .any(|tag| tag == current_etag)
}

/// Weak `ETag` comparison, per RFC 7232 §3.2 (`If-None-Match`).
///
/// `*` matches any existing object, so it is false when the object does not exist —
/// which is what makes `If-None-Match: *` a create-only precondition.
#[must_use]
pub fn matches_if_none_match(current_etag: Option<&str>, if_none_match_value: &str) -> bool {
    let Some(current) = current_etag else {
        return false;
    };

    if if_none_match_value.trim() == "*" {
        return true;
    }

    if_none_match_value.split(',').map(str::trim).any(|tag| {
        let tag = tag.trim_start_matches("W/");
        let current = current.trim_start_matches("W/");
        tag == current
    })
}

/// Evaluate the preconditions that gate a write.
///
/// `state` is `None` when no object exists at the key yet.
///
/// `If-Modified-Since` and `If-Unmodified-Since` are **not** evaluated here. The S3 API
/// has no way to express either on a PUT or DELETE, so `notedthat-storage-s3` drops
/// them; a backend that honoured them would make the same request succeed on one
/// deployment and fail on another, which is precisely the divergence the shared
/// `Storage` contract exists to prevent. Callers log the drop, as the S3 adapter does.
pub fn evaluate_write_preconditions(
    state: Option<ObjectState<'_>>,
    conditionals: &ConditionalHeaders,
) -> Result<(), StorageError> {
    if let Some(if_match) = &conditionals.if_match
        && !state.is_some_and(|object| matches_if_match(object.etag, if_match))
    {
        return Err(StorageError::PreconditionFailed);
    }

    if let Some(if_none_match) = &conditionals.if_none_match
        && matches_if_none_match(state.map(|object| object.etag), if_none_match)
    {
        return Err(StorageError::PreconditionFailed);
    }

    Ok(())
}

/// Evaluate the preconditions that gate a read, in RFC 7232 §6 precedence order.
///
/// `If-Match` is checked before `If-None-Match`, so a request carrying both a failing
/// `If-Match` and a matching `If-None-Match` is a 412 rather than a 304.
pub fn evaluate_read_preconditions(
    state: ObjectState<'_>,
    conditionals: &ConditionalHeaders,
) -> Result<(), StorageError> {
    if let Some(if_match) = &conditionals.if_match
        && !matches_if_match(state.etag, if_match)
    {
        return Err(StorageError::PreconditionFailed);
    }

    if let Some(if_unmodified_since) = &conditionals.if_unmodified_since {
        let threshold = parse_http_date_or_err(if_unmodified_since)?;
        if unix_seconds(state.last_modified) > unix_seconds(threshold) {
            return Err(StorageError::PreconditionFailed);
        }
    }

    if let Some(if_none_match) = &conditionals.if_none_match
        && matches_if_none_match(Some(state.etag), if_none_match)
    {
        return Err(StorageError::NotModified);
    }

    if let Some(if_modified_since) = &conditionals.if_modified_since {
        let threshold = parse_http_date_or_err(if_modified_since)?;
        if unix_seconds(state.last_modified) <= unix_seconds(threshold) {
            return Err(StorageError::NotModified);
        }
    }

    Ok(())
}

/// Resolve a requested byte range against an object of `total_size` bytes.
///
/// Returns the exclusive byte range together with the `Content-Range` value to report,
/// or `Ok(None)` when no range was requested and the whole body should be served.
///
/// Only the first range is honoured. [`crate::storage::ObjectRead`] carries a single
/// `content_range`, so the surfaces above cannot render a `multipart/byteranges`
/// response even if a backend produced one.
pub fn resolve_range(
    total_size: u64,
    ranges: Option<&[ByteRange]>,
) -> Result<Option<(Range<u64>, String)>, StorageError> {
    let Some(byte_range) = ranges.and_then(<[ByteRange]>::first) else {
        return Ok(None);
    };

    let exclusive =
        byte_range
            .to_exclusive_range(total_size)
            .ok_or(StorageError::RangeNotSatisfiable {
                complete_length: total_size,
            })?;

    let content_range = format!(
        "bytes {}-{}/{}",
        exclusive.start,
        exclusive.end - 1,
        total_size
    );
    Ok(Some((exclusive, content_range)))
}

#[cfg(test)]
mod tests {
    use super::{
        ObjectState, evaluate_read_preconditions, evaluate_write_preconditions, matches_if_match,
        matches_if_none_match, parse_http_date_or_err, resolve_range,
    };
    use crate::conditional::ConditionalHeaders;
    use crate::error::StorageError;
    use crate::range::ByteRange;
    use std::time::{Duration, UNIX_EPOCH};

    const ETAG: &str = "\"abc\"";

    fn state() -> ObjectState<'static> {
        ObjectState {
            etag: ETAG,
            last_modified: UNIX_EPOCH + Duration::from_secs(1_000),
        }
    }

    fn http_date(secs: u64) -> String {
        httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn if_match_accepts_star_and_list_members_but_not_weak_tags() {
        assert!(matches_if_match(ETAG, "*"));
        assert!(matches_if_match(ETAG, "\"other\", \"abc\""));
        assert!(!matches_if_match(ETAG, "\"other\""));
        assert!(!matches_if_match(ETAG, "W/\"abc\""));
    }

    #[test]
    fn if_none_match_star_is_false_for_a_missing_object() {
        assert!(!matches_if_none_match(None, "*"));
        assert!(matches_if_none_match(Some(ETAG), "*"));
    }

    #[test]
    fn if_none_match_compares_weakly() {
        assert!(matches_if_none_match(Some(ETAG), "W/\"abc\""));
    }

    #[test]
    fn write_preconditions_ignore_both_date_headers() {
        let conditionals = ConditionalHeaders {
            if_unmodified_since: Some(http_date(0)),
            if_modified_since: Some("not-a-date".into()),
            ..ConditionalHeaders::default()
        };
        assert!(evaluate_write_preconditions(Some(state()), &conditionals).is_ok());
    }

    #[test]
    fn write_if_match_fails_when_no_object_exists() {
        let conditionals = ConditionalHeaders {
            if_match: Some("*".into()),
            ..ConditionalHeaders::default()
        };
        assert!(matches!(
            evaluate_write_preconditions(None, &conditionals),
            Err(StorageError::PreconditionFailed)
        ));
    }

    #[test]
    fn read_if_match_is_evaluated_before_if_none_match() {
        let conditionals = ConditionalHeaders {
            if_match: Some("\"stale\"".into()),
            if_none_match: Some(ETAG.into()),
            ..ConditionalHeaders::default()
        };
        assert!(matches!(
            evaluate_read_preconditions(state(), &conditionals),
            Err(StorageError::PreconditionFailed)
        ));
    }

    #[test]
    fn read_if_modified_since_at_the_mtime_is_not_modified() {
        let conditionals = ConditionalHeaders {
            if_modified_since: Some(http_date(1_000)),
            ..ConditionalHeaders::default()
        };
        assert!(matches!(
            evaluate_read_preconditions(state(), &conditionals),
            Err(StorageError::NotModified)
        ));
    }

    #[test]
    fn read_rejects_a_malformed_date() {
        let conditionals = ConditionalHeaders {
            if_modified_since: Some("not-a-date".into()),
            ..ConditionalHeaders::default()
        };
        assert!(matches!(
            evaluate_read_preconditions(state(), &conditionals),
            Err(StorageError::Other { .. })
        ));
        assert!(parse_http_date_or_err("not-a-date").is_err());
    }

    #[test]
    fn resolve_range_reports_an_inclusive_content_range() {
        let (exclusive, content_range) = resolve_range(
            100,
            Some(&[ByteRange::FromStart {
                first: 10,
                last: 19,
            }]),
        )
        .expect("satisfiable")
        .expect("a range was requested");
        assert_eq!(exclusive, 10..20);
        assert_eq!(content_range, "bytes 10-19/100");
    }

    #[test]
    fn resolve_range_honours_only_the_first_range() {
        let (exclusive, _) = resolve_range(
            100,
            Some(&[
                ByteRange::FromStart { first: 0, last: 9 },
                ByteRange::FromStart {
                    first: 50,
                    last: 59,
                },
            ]),
        )
        .expect("satisfiable")
        .expect("a range was requested");
        assert_eq!(exclusive, 0..10);
    }

    #[test]
    fn resolve_range_reports_the_complete_length_when_unsatisfiable() {
        assert!(matches!(
            resolve_range(
                100,
                Some(&[ByteRange::FromStart {
                    first: 200,
                    last: 300
                }])
            ),
            Err(StorageError::RangeNotSatisfiable {
                complete_length: 100
            })
        ));
    }

    #[test]
    fn resolve_range_without_a_range_serves_the_whole_body() {
        assert!(resolve_range(100, None).expect("ok").is_none());
        assert!(resolve_range(100, Some(&[])).expect("ok").is_none());
    }
}
