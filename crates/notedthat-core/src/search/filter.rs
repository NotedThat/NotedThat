use crate::okf::{OkfStatus, OkfTrust};
use serde::{Deserialize, Serialize};

/// Filters applied to a search request. All fields are optional and AND-composed.
///
/// Unknown JSON fields are silently ignored (no `deny_unknown_fields`) for forward compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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
    /// Populated from OKF frontmatter `tags` (D48); empty for non-OKF documents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Only return hits whose OKF `type` equals this value exactly.
    ///
    /// Case-sensitive: OKF puts no registry behind `type`, so normalising it here
    /// would silently merge distinct concept kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub okf_type: Option<String>,

    /// Only return hits with this OKF lifecycle status.
    ///
    /// Documents with no `status` key are indexed as `stable`, so `stable` matches them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub okf_status: Option<OkfStatus>,

    /// Only return hits at or above this trust tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub okf_min_trust: Option<OkfTrust>,

    /// Only return hits whose Attested Computation `runtime` equals this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub okf_runtime: Option<String>,

    /// Only return hits from a chunk of this kind — `"body"` or `"metadata"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_kind: Option<String>,

    /// Exclude hits whose `stale_after` has passed.
    ///
    /// Defaults to `false`: search returns everything and annotates staleness, per
    /// OKF §11 ("never reject a concept for missing trust data") and D48.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub exclude_stale: bool,

    /// Only return hits from documents that carry OKF frontmatter at all.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub okf_only: bool,
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
            && self.okf_type.is_none()
            && self.okf_status.is_none()
            && self.okf_min_trust.is_none()
            && self.okf_runtime.is_none()
            && self.chunk_kind.is_none()
            && !self.exclude_stale
            && !self.okf_only
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
    #[test]
    fn okf_type_sets_not_empty() {
        let f = SearchFilter {
            okf_type: Some("Metric".into()),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn okf_status_sets_not_empty() {
        let f = SearchFilter {
            okf_status: Some(OkfStatus::Draft),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn okf_min_trust_sets_not_empty() {
        let f = SearchFilter {
            okf_min_trust: Some(OkfTrust::HumanReviewed),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn okf_runtime_sets_not_empty() {
        let f = SearchFilter {
            okf_runtime: Some("bigquery".into()),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn chunk_kind_sets_not_empty() {
        let f = SearchFilter {
            chunk_kind: Some("metadata".into()),
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn exclude_stale_sets_not_empty() {
        let f = SearchFilter {
            exclude_stale: true,
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn okf_only_sets_not_empty() {
        let f = SearchFilter {
            okf_only: true,
            ..Default::default()
        };
        assert!(!f.is_empty());
    }

    #[test]
    fn default_filter_serialises_to_an_empty_object() {
        // Regression guard: a missing skip_serializing_if on any new field shows up here.
        assert_eq!(
            serde_json::to_string(&SearchFilter::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn okf_fields_round_trip_through_json() {
        let f = SearchFilter {
            okf_type: Some("Attested Computation".into()),
            okf_status: Some(OkfStatus::Deprecated),
            okf_min_trust: Some(OkfTrust::MachineConfirmed),
            okf_runtime: Some("dbt".into()),
            chunk_kind: Some("body".into()),
            exclude_stale: true,
            okf_only: true,
            ..Default::default()
        };
        let back: SearchFilter = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn unknown_fields_are_still_ignored() {
        let f: SearchFilter = serde_json::from_str(r#"{"okf_type":"Metric","future":1}"#).unwrap();
        assert_eq!(f.okf_type.as_deref(), Some("Metric"));
    }
}
