//! The parsed OKF concept model.
//!
//! # Non-execution invariant
//!
//! [`OkfComputation`] catalogues an Attested Computation. `computation`,
//! `executor.resource` and `attester.resource` are opaque strings: nothing in
//! this crate resolves, fetches or executes them. See SPECIFICATIONS.md D48.

use notedthat_core::okf::{Actor, OkfAnnotation, OkfInstant, OkfStatus, OkfTrust};
use serde::{Deserialize, Serialize};

/// Maximum characters kept for `okf_title` in the Qdrant payload.
///
/// The annotation is copied onto every chunk of a document, so an uncapped title
/// on a forty-chunk file is forty copies of it.
pub const MAX_TITLE_CHARS: usize = 200;

/// Maximum characters kept for `okf_description` in the Qdrant payload.
pub const MAX_DESCRIPTION_CHARS: usize = 500;

/// A parsed OKF v0.2 concept: the frontmatter of one non-reserved `.md` file.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfConcept {
    /// The required `type`. A free string with no central registry.
    pub concept_type: String,
    /// Human-readable display name.
    pub title: Option<String>,
    /// Single-sentence summary.
    pub description: Option<String>,
    /// URI uniquely identifying the underlying asset.
    pub resource: Option<String>,
    /// Cross-cutting categorisation.
    pub tags: Vec<String>,
    /// Provenance entries.
    pub sources: Vec<OkfSource>,
    /// The window the `sources` credibility signals were measured over.
    pub usage_window: Option<OkfWindow>,
    /// Who or what produced the content, and when.
    pub generated: Option<OkfGenerated>,
    /// Verification events. A bare mapping in YAML is normalised to one element.
    pub verified: Vec<OkfVerification>,
    /// The stated lifecycle status. `None` means the key was absent, which OKF
    /// defines as [`OkfStatus::Stable`] — see [`OkfConcept::effective_status`].
    pub status: Option<OkfStatus>,
    /// The instant at which the content becomes stale.
    pub stale_after: Option<OkfInstant>,
    /// The Attested Computation family, extracted whenever present regardless of
    /// `type` — a producer may spell the type differently than the spec's example.
    pub computation: Option<OkfComputation>,
}

/// One provenance entry from the `sources` family.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfSource {
    /// The URL or path of the source. Required within an entry.
    pub resource: String,
    /// Stable key used as a Markdown footnote label for per-claim attribution.
    pub id: Option<String>,
    /// Human-readable label.
    pub title: Option<String>,
    /// Credibility signal: who wrote it.
    pub author: Option<Actor>,
    /// Credibility signal: how often it was used.
    pub usage_count: Option<u64>,
    /// Credibility signal: when it last changed.
    pub last_modified: Option<OkfInstant>,
}

/// The `usage_window` over which `sources` credibility signals were measured.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfWindow {
    /// Start of the window.
    pub from: Option<OkfInstant>,
    /// End of the window.
    pub to: Option<OkfInstant>,
}

/// The `generated` family: content origin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfGenerated {
    /// Who or what produced the content.
    pub by: Option<Actor>,
    /// When the content last meaningfully changed.
    pub at: Option<OkfInstant>,
}

/// One entry of the `verified` family: an independent confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfVerification {
    /// Who confirmed it.
    pub by: Option<Actor>,
    /// When they confirmed it.
    pub at: Option<OkfInstant>,
}

/// The Attested Computation family. **Catalogued, never executed.**
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfComputation {
    /// How parameters bind: `bigquery`, `postgres`, `dbt`, `python`, `Looker`, …
    pub runtime: Option<String>,
    /// Typed, named holes an agent may fill. The computation itself is not editable.
    pub parameters: Vec<OkfParameter>,
    /// Path to the computation source. Opaque here — never resolved or fetched.
    pub computation: Option<String>,
    /// How to run it. Opaque here.
    pub executor: Option<OkfExecutor>,
    /// How to check the receipt. Opaque here.
    pub attester: Option<OkfAttester>,
}

/// One parameter of an Attested Computation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfParameter {
    /// Parameter name.
    pub name: String,
    /// Declared type, verbatim.
    pub param_type: Option<String>,
    /// Whether the parameter must be supplied.
    pub required: Option<bool>,
}

/// The `executor` family of an Attested Computation. **Never resolved.**
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfExecutor {
    /// Path to the execution instructions, verbatim.
    pub resource: Option<String>,
    /// Fields a run must return for attestation.
    pub receipt: Vec<String>,
}

/// The `attester` family of an Attested Computation. **Never resolved.**
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkfAttester {
    /// Path to the deterministic checking code, verbatim.
    pub resource: Option<String>,
}

impl OkfConcept {
    /// Trust tier derived from `verified`.
    ///
    /// A pure function of the document, so it is safe to bake in at index time.
    #[must_use]
    pub fn trust(&self) -> OkfTrust {
        OkfTrust::derive(self.verified.iter().map(|v| v.by.as_ref()))
    }

    /// The effective status: what was stated, else [`OkfStatus::Stable`].
    #[must_use]
    pub fn effective_status(&self) -> OkfStatus {
        self.status.unwrap_or(OkfStatus::Stable)
    }

    /// Whether the concept is stale at `now_unix`.
    ///
    /// Evaluated per request rather than baked in at index time, because
    /// `stale_after` is an absolute instant that the clock walks past.
    #[must_use]
    pub fn is_stale(&self, now_unix: i64) -> bool {
        self.stale_after
            .as_ref()
            .is_some_and(|i| i.has_passed(now_unix))
    }

    /// Whether this concept declares an Attested Computation runtime.
    #[must_use]
    pub fn runtime(&self) -> Option<&str> {
        self.computation.as_ref()?.runtime.as_deref()
    }

    /// Build the wire annotation for a search hit evaluated at `now_unix`.
    #[must_use]
    pub fn to_annotation(&self, now_unix: i64) -> OkfAnnotation {
        let mut a = OkfAnnotation::new(&self.concept_type);
        a.title = self
            .title
            .as_deref()
            .map(|t| truncate_chars(t, MAX_TITLE_CHARS));
        a.description = self
            .description
            .as_deref()
            .map(|d| truncate_chars(d, MAX_DESCRIPTION_CHARS));
        a.resource.clone_from(&self.resource);
        a.tags.clone_from(&self.tags);
        a.status = self.effective_status();
        a.trust = self.trust();
        a.stale = self.is_stale(now_unix);
        a.stale_after = self.stale_after.as_ref().map(|i| i.raw.clone());
        a.runtime = self.runtime().map(ToOwned::to_owned);
        a
    }
}

/// Truncate to at most `max` characters, never splitting a character.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concept(concept_type: &str) -> OkfConcept {
        OkfConcept {
            concept_type: concept_type.into(),
            ..Default::default()
        }
    }

    #[test]
    fn absent_status_is_effectively_stable() {
        assert_eq!(concept("Metric").effective_status(), OkfStatus::Stable);
    }

    #[test]
    fn stated_status_wins() {
        let mut c = concept("Metric");
        c.status = Some(OkfStatus::Draft);
        assert_eq!(c.effective_status(), OkfStatus::Draft);
    }

    #[test]
    fn no_verified_entries_is_unverified() {
        assert_eq!(concept("Metric").trust(), OkfTrust::Unverified);
    }

    #[test]
    fn a_human_verification_is_human_reviewed() {
        let mut c = concept("Metric");
        c.verified = vec![OkfVerification {
            by: Some(Actor::new("human:ahormati")),
            at: None,
        }];
        assert_eq!(c.trust(), OkfTrust::HumanReviewed);
    }

    #[test]
    fn absent_stale_after_is_never_stale() {
        assert!(!concept("Metric").is_stale(i64::MAX));
    }

    #[test]
    fn unparseable_stale_after_is_never_stale() {
        let mut c = concept("Metric");
        c.stale_after = Some(OkfInstant::new("someday", None));
        assert!(!c.is_stale(i64::MAX));
    }

    #[test]
    fn stale_exactly_at_the_instant() {
        let mut c = concept("Metric");
        c.stale_after = Some(OkfInstant::new("2026-01-01", Some(1_000)));
        assert!(c.is_stale(1_000));
        assert!(!c.is_stale(999));
    }

    #[test]
    fn annotation_echoes_the_lexical_stale_after() {
        let mut c = concept("Metric");
        c.stale_after = Some(OkfInstant::new("2026-12-31", Some(1_000)));
        let a = c.to_annotation(2_000);
        assert_eq!(a.stale_after.as_deref(), Some("2026-12-31"));
        assert!(a.stale);
    }

    #[test]
    fn annotation_truncates_title_and_description() {
        let mut c = concept("Metric");
        c.title = Some("t".repeat(MAX_TITLE_CHARS + 50));
        c.description = Some("d".repeat(MAX_DESCRIPTION_CHARS + 50));
        let a = c.to_annotation(0);
        assert_eq!(a.title.unwrap().chars().count(), MAX_TITLE_CHARS);
        assert_eq!(
            a.description.unwrap().chars().count(),
            MAX_DESCRIPTION_CHARS
        );
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let s = "\u{1f680}".repeat(10);
        let out = truncate_chars(&s, 3);
        assert_eq!(out.chars().count(), 3);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn runtime_is_read_through_the_computation_family() {
        let mut c = concept("Attested Computation");
        c.computation = Some(OkfComputation {
            runtime: Some("bigquery".into()),
            ..Default::default()
        });
        assert_eq!(c.runtime(), Some("bigquery"));
        assert_eq!(c.to_annotation(0).runtime.as_deref(), Some("bigquery"));
    }
}
