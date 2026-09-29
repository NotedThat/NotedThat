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
use crate::error::{StorageError, Validators};
use crate::range::ByteRange;

/// The two facts about a stored object that every precondition check needs.
#[derive(Debug, Clone, Copy)]
pub struct ObjectState<'a> {
    /// The object's current `ETag`, quoted per RFC 7232 §2.3.
    pub etag: &'a str,
    /// The object's last modification time.
    pub last_modified: SystemTime,
}

impl ObjectState<'_> {
    /// The 304 this object answers, carrying its validators (RFC 9110 §15.4.5).
    fn not_modified(self) -> StorageError {
        StorageError::NotModified(Validators {
            etag: Some(self.etag.to_string()),
            last_modified: Some(unix_seconds_i64(self.last_modified)),
        })
    }
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
///
/// # Errors
///
/// Returns [`StorageError::Other`] wrapping an
/// [`InvalidInput`](std::io::ErrorKind::InvalidInput) I/O error when `value` is not an
/// HTTP-date.
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
///
/// # Errors
///
/// Returns [`StorageError::PreconditionFailed`] when `If-Match` names no current object's
/// `ETag`, or when `If-None-Match` matches the current object (`*` matches any existing
/// one).
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

/// Evaluate the preconditions that gate a read, in RFC 9110 §13.2.2 precedence order.
///
/// `If-Match` is checked before `If-None-Match`, so a request carrying both a failing
/// `If-Match` and a matching `If-None-Match` is a 412 rather than a 304. A date header
/// whose `ETag` counterpart is present is ignored, unparsed ([`ConditionalHeaders::for_read`]).
///
/// # Errors
///
/// Returns [`StorageError::PreconditionFailed`] when `If-Match` does not match or the
/// object changed after `If-Unmodified-Since`; [`StorageError::NotModified`], carrying the
/// object's validators, when `If-None-Match` matches or the object is unchanged since
/// `If-Modified-Since`; and the [`parse_http_date_or_err`] error when a date header that is
/// evaluated is malformed.
pub fn evaluate_read_preconditions(
    state: ObjectState<'_>,
    conditionals: &ConditionalHeaders,
) -> Result<(), StorageError> {
    let conditionals = &conditionals.for_read();

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
        return Err(state.not_modified());
    }

    if let Some(if_modified_since) = &conditionals.if_modified_since {
        let threshold = parse_http_date_or_err(if_modified_since)?;
        if unix_seconds(state.last_modified) <= unix_seconds(threshold) {
            return Err(state.not_modified());
        }
    }

    Ok(())
}

/// Whether an `If-Range` validator still describes the object, per RFC 9110 §13.1.5.
///
/// `etag` and `last_modified` (Unix seconds) are those of the representation the range
/// would be cut from. An entity tag must match by the strong comparison, so a weak tag
/// never matches; an HTTP-date must equal `Last-Modified` exactly. Anything else,
/// including a value that is neither, is a mismatch, and the caller then ignores `Range`
/// and serves the whole representation.
#[must_use]
pub fn if_range_matches(if_range: &str, etag: Option<&str>, last_modified: Option<i64>) -> bool {
    let if_range = if_range.trim();
    if if_range.starts_with('"') || if_range.starts_with("W/") {
        return !if_range.starts_with("W/")
            && etag.is_some_and(|current| !current.starts_with("W/") && current == if_range);
    }
    httpdate::parse_http_date(if_range)
        .is_ok_and(|date| last_modified == Some(unix_seconds_i64(date)))
}

/// Resolve a requested byte range against an object of `total_size` bytes.
///
/// Returns the exclusive byte range together with the `Content-Range` value to report,
/// or `Ok(None)` when no range was requested and the whole body should be served.
///
/// # Errors
///
/// Returns [`StorageError::RangeNotSatisfiable`] when the requested range lies outside an
/// object of `total_size` bytes.
pub fn resolve_range(
    total_size: u64,
    range: Option<&ByteRange>,
) -> Result<Option<(Range<u64>, String)>, StorageError> {
    let Some(byte_range) = range else {
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
        ObjectState, evaluate_read_preconditions, evaluate_write_preconditions, if_range_matches,
        matches_if_match, matches_if_none_match, parse_http_date_or_err, resolve_range,
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
        let Err(StorageError::NotModified(validators)) =
            evaluate_read_preconditions(state(), &conditionals)
        else {
            panic!("expected NotModified");
        };
        assert_eq!(validators.etag.as_deref(), Some(ETAG));
        assert_eq!(validators.last_modified, Some(1_000));
    }

    #[test]
    fn read_ignores_if_unmodified_since_when_if_match_is_present() {
        let conditionals = ConditionalHeaders {
            if_match: Some(ETAG.into()),
            if_unmodified_since: Some(http_date(0)),
            ..ConditionalHeaders::default()
        };
        assert!(evaluate_read_preconditions(state(), &conditionals).is_ok());
    }

    #[test]
    fn read_ignores_if_modified_since_when_if_none_match_is_present() {
        // A changed ETag with a same-second date: the ETag decides, so no stale 304.
        let conditionals = ConditionalHeaders {
            if_none_match: Some("\"stale\"".into()),
            if_modified_since: Some(http_date(1_000)),
            ..ConditionalHeaders::default()
        };
        assert!(evaluate_read_preconditions(state(), &conditionals).is_ok());
    }

    #[test]
    fn read_does_not_parse_a_superseded_date() {
        let conditionals = ConditionalHeaders {
            if_match: Some(ETAG.into()),
            if_none_match: Some("\"stale\"".into()),
            if_modified_since: Some("not-a-date".into()),
            if_unmodified_since: Some("not-a-date".into()),
        };
        assert!(evaluate_read_preconditions(state(), &conditionals).is_ok());
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
    fn if_range_entity_tags_compare_strongly() {
        assert!(if_range_matches(ETAG, Some(ETAG), None));
        assert!(!if_range_matches("\"stale\"", Some(ETAG), None));
        assert!(!if_range_matches("W/\"abc\"", Some(ETAG), None));
        assert!(!if_range_matches("W/\"abc\"", Some("W/\"abc\""), None));
        assert!(!if_range_matches(ETAG, None, Some(1_000)));
    }

    #[test]
    fn if_range_dates_must_equal_last_modified() {
        assert!(if_range_matches(&http_date(1_000), None, Some(1_000)));
        assert!(!if_range_matches(&http_date(999), None, Some(1_000)));
        assert!(!if_range_matches(&http_date(1_001), None, Some(1_000)));
        assert!(!if_range_matches(&http_date(1_000), None, None));
        assert!(!if_range_matches("not-a-date", Some(ETAG), Some(1_000)));
    }

    #[test]
    fn resolve_range_reports_an_inclusive_content_range() {
        let (exclusive, content_range) = resolve_range(
            100,
            Some(&ByteRange::FromStart {
                first: 10,
                last: 19,
            }),
        )
        .expect("satisfiable")
        .expect("a range was requested");
        assert_eq!(exclusive, 10..20);
        assert_eq!(content_range, "bytes 10-19/100");
    }

    #[test]
    fn resolve_range_reports_the_complete_length_when_unsatisfiable() {
        assert!(matches!(
            resolve_range(
                100,
                Some(&ByteRange::FromStart {
                    first: 200,
                    last: 300
                })
            ),
            Err(StorageError::RangeNotSatisfiable {
                complete_length: 100
            })
        ));
    }

    #[test]
    fn resolve_range_without_a_range_serves_the_whole_body() {
        assert!(resolve_range(100, None).expect("ok").is_none());
    }
}
