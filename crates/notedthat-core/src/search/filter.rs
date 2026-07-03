use serde::{Deserialize, Serialize};

/// Filters applied to a search request. All fields are optional and AND-composed.
///
/// Unknown JSON fields are silently ignored (no `deny_unknown_fields`) for forward compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFilter {
    /// Only return hits whose `object_key` starts with this prefix (client-side post-filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_key_prefix: Option<String>,

    /// Only return hits with exactly this MIME type (e.g. `"text/markdown"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,

    /// Only return hits whose `heading_path` array starts with these segments (prefix match).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path_prefix: Vec<String>,

    /// Only return hits with `mtime >= updated_after` (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<i64>,

    /// Only return hits with `mtime <= updated_before` (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_before: Option<i64>,

    /// Only return hits tagged with at least one of these tags.
    /// Reserved shape — tags are not populated in M5 (D33).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl SearchFilter {
    /// Returns true iff every field is `None` or empty, i.e. no filtering will be applied.
    pub fn is_empty(&self) -> bool {
        self.object_key_prefix.is_none()
            && self.mime.is_none()
            && self.heading_path_prefix.is_empty()
            && self.updated_after.is_none()
            && self.updated_before.is_none()
            && self.tags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        assert!(SearchFilter::default().is_empty());
    }

    #[test]
    fn object_key_prefix_sets_not_empty() {
        let f = SearchFilter {
            object_key_prefix: Some("docs/".into()),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn mime_sets_not_empty() {
        let f = SearchFilter {
            mime: Some("text/markdown".into()),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn heading_path_prefix_sets_not_empty() {
        let f = SearchFilter {
            heading_path_prefix: vec!["Intro".into()],
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn updated_after_sets_not_empty() {
        let f = SearchFilter {
            updated_after: Some(1_700_000_000),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn updated_before_sets_not_empty() {
        let f = SearchFilter {
            updated_before: Some(1_800_000_000),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn tags_sets_not_empty() {
        let f = SearchFilter {
            tags: vec!["rust".into()],
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn serde_round_trip_empty_object() {
        let f = SearchFilter::default();
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, "{}");
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn serde_round_trip_populated() {
        let f = SearchFilter {
            mime: Some("text/markdown".into()),
            heading_path_prefix: vec!["A".into(), "B".into()],
            updated_after: Some(1_700_000_000),
            ..Default::default()
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn deserialize_missing_fields_yields_defaults() {
        let f: SearchFilter = serde_json::from_str("{}").unwrap();
        assert_eq!(f, SearchFilter::default());
    }

    #[test]
    fn deserialize_null_yields_default() {
        // Note: SearchFilter itself is not nullable, but serde(default) handles absence of individual fields
        let f: SearchFilter = serde_json::from_str(r#"{"mime":null}"#).unwrap();
        assert_eq!(f.mime, None);
    }

    #[test]
    fn deserialize_ignores_unknown_fields() {
        let f: SearchFilter =
            serde_json::from_str(r#"{"mime":"text/plain","unknown_field":true}"#).unwrap();
        assert_eq!(f.mime, Some("text/plain".into()));
    }
}
