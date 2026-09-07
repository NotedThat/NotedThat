//! ISO 8601 instants as they appear in OKF frontmatter.

use serde::{Deserialize, Serialize};

/// An ISO 8601 instant read from OKF frontmatter.
///
/// `raw` is always the original lexical form, so a client can re-evaluate it
/// against its own clock and its own parser. `epoch_secs` is `Some` only when the
/// value was understood; an instant we cannot parse is never treated as expired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkfInstant {
    /// The value exactly as written in the document.
    pub raw: String,
    /// Unix seconds, when the lexical form was understood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_secs: Option<i64>,
}

impl OkfInstant {
    /// Build an instant from its lexical form and an optional resolved epoch.
    pub fn new(raw: impl Into<String>, epoch_secs: Option<i64>) -> Self {
        Self {
            raw: raw.into(),
            epoch_secs,
        }
    }

    /// Whether `now_unix` is at or past this instant.
    ///
    /// An unparseable instant is never past — OKF §11 forbids penalising a concept
    /// for a malformed optional field.
    #[must_use]
    pub fn has_passed(&self, now_unix: i64) -> bool {
        self.epoch_secs.is_some_and(|t| now_unix >= t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unparseable_instant_never_passes() {
        let i = OkfInstant::new("not a date", None);
        assert!(!i.has_passed(i64::MAX));
    }

    #[test]
    fn exactly_at_the_instant_counts_as_passed() {
        // OKF defines staleness as `now >= stale_after`, not `>`.
        let i = OkfInstant::new("2026-01-01T00:00:00Z", Some(1_767_225_600));
        assert!(i.has_passed(1_767_225_600));
    }

    #[test]
    fn one_second_before_has_not_passed() {
        let i = OkfInstant::new("2026-01-01T00:00:00Z", Some(1_767_225_600));
        assert!(!i.has_passed(1_767_225_599));
    }

    #[test]
    fn epoch_omitted_from_json_when_absent() {
        let json = serde_json::to_string(&OkfInstant::new("2026-12-31", None)).unwrap();
        assert_eq!(json, r#"{"raw":"2026-12-31"}"#);
    }
}
