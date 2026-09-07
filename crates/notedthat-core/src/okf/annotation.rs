//! `OkfAnnotation` — the OKF metadata carried on a search hit.

use super::{OkfStatus, OkfTrust};
use serde::{Deserialize, Serialize};

/// OKF v0.2 metadata attached to a [`crate::search::SearchHit`].
///
/// Present only when the source document carried conformant OKF frontmatter.
/// Per OKF §11 a consumer must tolerate unknown `concept_type` values and absent
/// optional families, so every optional field here is genuinely optional.
///
/// # Non-execution invariant
///
/// `runtime` describes an Attested Computation so that agents can *discover* it.
/// `NotedThat` never resolves, fetches or executes a computation, and never writes
/// attestation receipts. See SPECIFICATIONS.md D48.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkfAnnotation {
    /// The OKF `type`. A free string with no central registry.
    #[serde(rename = "type")]
    pub concept_type: String,
    /// Human-readable display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Single-sentence summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// URI identifying the underlying asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Cross-cutting tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Effective lifecycle status.
    ///
    /// Always present: an absent `status` key is reported as `stable`, so clients
    /// never have to reimplement the defaulting rule.
    pub status: OkfStatus,
    /// Trust tier derived from `verified` at index time.
    ///
    /// Always present, for the same reason as `status`.
    pub trust: OkfTrust,
    /// Whether `stale_after` has passed, evaluated per request rather than baked
    /// in at index time.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub stale: bool,
    /// `stale_after` echoed so a client can re-evaluate against its own clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after: Option<String>,
    /// `runtime` of an Attested Computation. Catalogue only — never executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

impl OkfAnnotation {
    /// A minimal annotation carrying only the required `type`.
    pub fn new(concept_type: impl Into<String>) -> Self {
        Self {
            concept_type: concept_type.into(),
            title: None,
            description: None,
            resource: None,
            tags: Vec::new(),
            status: OkfStatus::Stable,
            trust: OkfTrust::Unverified,
            stale: false,
            stale_after: None,
            runtime: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_annotation_serialises_required_fields_only() {
        let json = serde_json::to_string(&OkfAnnotation::new("Metric")).unwrap();
        assert_eq!(
            json,
            r#"{"type":"Metric","status":"stable","trust":"unverified"}"#
        );
    }

    #[test]
    fn status_and_trust_are_never_omitted() {
        // They are meaningful values, not absences; omitting them would force every
        // client to reimplement the defaulting rules.
        let json = serde_json::to_string(&OkfAnnotation::new("Playbook")).unwrap();
        assert!(json.contains("\"status\""));
        assert!(json.contains("\"trust\""));
    }

    #[test]
    fn false_stale_is_omitted() {
        let json = serde_json::to_string(&OkfAnnotation::new("Metric")).unwrap();
        assert!(!json.contains("stale"));
    }

    #[test]
    fn unknown_keys_are_ignored_on_deserialize() {
        let json = r#"{"type":"Metric","status":"stable","trust":"unverified","future_field":1}"#;
        let a: OkfAnnotation = serde_json::from_str(json).unwrap();
        assert_eq!(a.concept_type, "Metric");
    }

    #[test]
    fn full_annotation_round_trips() {
        let mut a = OkfAnnotation::new("Attested Computation");
        a.title = Some("Daily Revenue".into());
        a.tags = vec!["finance".into()];
        a.trust = OkfTrust::HumanReviewed;
        a.stale = true;
        a.stale_after = Some("2020-01-01T00:00:00Z".into());
        a.runtime = Some("bigquery".into());
        let back: OkfAnnotation =
            serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn send_sync_clone_debug() {
        fn assert<T: Send + Sync + Clone + std::fmt::Debug>() {}
        assert::<OkfAnnotation>();
    }
}
