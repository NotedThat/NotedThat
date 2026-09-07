//! Parsing the ISO 8601 instants that OKF frontmatter carries.

use notedthat_core::okf::OkfInstant;
use time::{Date, OffsetDateTime, PrimitiveDateTime};

/// Parse an ISO 8601 instant to Unix seconds.
///
/// Accepts RFC 3339 (`2026-01-01T00:00:00Z`, `2026-01-01T02:00:00+02:00`) and a
/// bare `YYYY-MM-DD` date, which is read as midnight UTC. The date-only form
/// matters: `stale_after: 2026-12-31` is valid ISO 8601, is *not* valid RFC 3339,
/// and is exactly what a person writes by hand.
///
/// Anything else yields `None`. Per OKF §11 the caller keeps the lexical form
/// either way and simply never treats the value as having passed.
#[must_use]
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(dt) = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
        return Some(dt.unix_timestamp());
    }
    // A date-time with no offset is read as UTC.
    if let Ok(dt) = PrimitiveDateTime::parse(
        s,
        time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second][optional [.[subsecond]]]"
        ),
    ) {
        return Some(dt.assume_utc().unix_timestamp());
    }
    if let Ok(date) = Date::parse(s, time::macros::format_description!("[year]-[month]-[day]")) {
        return Some(date.midnight().assume_utc().unix_timestamp());
    }
    None
}

/// Build an [`OkfInstant`] from a lexical form, resolving it where possible.
#[must_use]
pub fn instant(raw: &str) -> OkfInstant {
    OkfInstant::new(raw, parse_iso8601(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_utc() {
        assert_eq!(parse_iso8601("2026-01-01T00:00:00Z"), Some(1_767_225_600));
    }

    #[test]
    fn parses_rfc3339_with_positive_offset() {
        assert_eq!(
            parse_iso8601("2026-01-01T02:00:00+02:00"),
            Some(1_767_225_600)
        );
    }

    #[test]
    fn parses_rfc3339_with_negative_offset() {
        assert_eq!(
            parse_iso8601("2025-12-31T19:00:00-05:00"),
            Some(1_767_225_600)
        );
    }

    #[test]
    fn parses_fractional_seconds() {
        assert_eq!(
            parse_iso8601("2026-01-01T00:00:00.500Z"),
            Some(1_767_225_600)
        );
    }

    #[test]
    fn parses_offsetless_datetime_as_utc() {
        assert_eq!(parse_iso8601("2026-01-01T00:00:00"), Some(1_767_225_600));
    }

    #[test]
    fn parses_a_bare_date_as_midnight_utc() {
        // The form a human actually writes, and not valid RFC 3339.
        assert_eq!(parse_iso8601("2026-01-01"), Some(1_767_225_600));
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(parse_iso8601("  2026-01-01  "), Some(1_767_225_600));
    }

    #[test]
    fn rejects_an_impossible_date() {
        assert_eq!(parse_iso8601("2026-13-45"), None);
    }

    #[test]
    fn rejects_an_empty_string() {
        assert_eq!(parse_iso8601(""), None);
    }

    #[test]
    fn rejects_a_unix_integer() {
        assert_eq!(parse_iso8601("1767225600"), None);
    }

    #[test]
    fn rejects_prose() {
        assert_eq!(parse_iso8601("next tuesday"), None);
    }

    #[test]
    fn instant_keeps_the_lexical_form_even_when_unparseable() {
        let i = instant("next tuesday");
        assert_eq!(i.raw, "next tuesday");
        assert_eq!(i.epoch_secs, None);
        assert!(!i.has_passed(i64::MAX));
    }
}
