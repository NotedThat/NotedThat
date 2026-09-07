//! Trust tiers and lifecycle status derived from OKF frontmatter.

use super::Actor;
use serde::{Deserialize, Serialize};

/// Trust tier derived from an OKF concept's `verified` family (OKF v0.2 §5.3).
///
/// The ordering is meaningful and is what `okf_min_trust` filters on:
/// `Unverified < MachineConfirmed < HumanReviewed`.
///
/// This is a pure function of the document, so it is derived once at index time.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OkfTrust {
    /// No `verified` key, or a `verified` family that yielded no usable entries.
    #[default]
    Unverified,
    /// Verified only by non-human actors.
    MachineConfirmed,
    /// Verified by at least one `human:<id>` actor.
    HumanReviewed,
}

impl OkfTrust {
    /// The wire and payload spelling of this tier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::MachineConfirmed => "machine_confirmed",
            Self::HumanReviewed => "human_reviewed",
        }
    }

    /// Every tier at or above `self`, ascending.
    ///
    /// Keyword payloads have no ordinal comparison in Qdrant, so `okf_min_trust`
    /// is expressed as a `MatchAny` over this set — which has at most three members.
    pub fn at_least(self) -> impl Iterator<Item = Self> {
        [
            Self::Unverified,
            Self::MachineConfirmed,
            Self::HumanReviewed,
        ]
        .into_iter()
        .filter(move |t| *t >= self)
    }

    /// Parse the wire spelling. Unknown values yield `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unverified" => Some(Self::Unverified),
            "machine_confirmed" => Some(Self::MachineConfirmed),
            "human_reviewed" => Some(Self::HumanReviewed),
            _ => None,
        }
    }

    /// Derive the tier from the actors that verified a concept.
    ///
    /// An entry with no `by` actor still counts as a verification event — it just
    /// cannot raise the tier to [`OkfTrust::HumanReviewed`].
    pub fn derive<'a>(verified_by: impl IntoIterator<Item = Option<&'a Actor>>) -> Self {
        let mut tier = Self::Unverified;
        for by in verified_by {
            if by.is_some_and(Actor::is_human) {
                return Self::HumanReviewed;
            }
            tier = Self::MachineConfirmed;
        }
        tier
    }
}

/// OKF lifecycle status (OKF v0.2 §6).
///
/// An absent `status` key means [`OkfStatus::Stable`], so this enum is only ever
/// used to represent a *stated* status; the defaulting lives in the concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum OkfStatus {
    /// Work in progress.
    Draft,
    /// The default when `status` is absent.
    #[default]
    Stable,
    /// Retained but no longer recommended.
    Deprecated,
}

impl OkfStatus {
    /// The wire and payload spelling of this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Stable => "stable",
            Self::Deprecated => "deprecated",
        }
    }

    /// Parse the closed vocabulary.
    ///
    /// Unknown values yield `None`, which the caller treats as "no status stated"
    /// and therefore as [`OkfStatus::Stable`] — OKF §11 forbids rejecting a concept
    /// over an unrecognised optional value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "stable" => Some(Self::Stable),
            "deprecated" => Some(Self::Deprecated),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human() -> Actor {
        Actor::new("human:ahormati")
    }
    fn machine() -> Actor {
        Actor::new("reference_agent/gemini-2.5-pro")
    }

    #[test]
    fn no_entries_is_unverified() {
        assert_eq!(OkfTrust::derive(std::iter::empty()), OkfTrust::Unverified);
    }

    #[test]
    fn only_machine_actors_is_machine_confirmed() {
        let m = machine();
        assert_eq!(
            OkfTrust::derive([Some(&m), Some(&m)]),
            OkfTrust::MachineConfirmed
        );
    }

    #[test]
    fn any_human_actor_is_human_reviewed() {
        let (m, h) = (machine(), human());
        assert_eq!(
            OkfTrust::derive([Some(&m), Some(&h)]),
            OkfTrust::HumanReviewed
        );
    }

    #[test]
    fn entry_without_actor_is_machine_confirmed() {
        assert_eq!(OkfTrust::derive([None]), OkfTrust::MachineConfirmed);
    }

    #[test]
    fn tier_ordering_is_ascending() {
        assert!(OkfTrust::Unverified < OkfTrust::MachineConfirmed);
        assert!(OkfTrust::MachineConfirmed < OkfTrust::HumanReviewed);
    }

    #[test]
    fn at_least_unverified_yields_all_three() {
        assert_eq!(OkfTrust::Unverified.at_least().count(), 3);
    }

    #[test]
    fn at_least_human_reviewed_yields_one() {
        let tiers: Vec<_> = OkfTrust::HumanReviewed.at_least().collect();
        assert_eq!(tiers, vec![OkfTrust::HumanReviewed]);
    }

    #[test]
    fn trust_wire_spelling_round_trips() {
        for t in OkfTrust::Unverified.at_least() {
            assert_eq!(OkfTrust::parse(t.as_str()), Some(t));
        }
        assert_eq!(OkfTrust::parse("nonsense"), None);
    }

    #[test]
    fn status_vocabulary_parses() {
        assert_eq!(OkfStatus::parse("draft"), Some(OkfStatus::Draft));
        assert_eq!(OkfStatus::parse("stable"), Some(OkfStatus::Stable));
        assert_eq!(OkfStatus::parse("deprecated"), Some(OkfStatus::Deprecated));
    }

    #[test]
    fn unknown_status_is_none_not_an_error() {
        assert_eq!(OkfStatus::parse("archived"), None);
    }

    #[test]
    fn status_parsing_is_case_sensitive() {
        assert_eq!(OkfStatus::parse("Stable"), None);
    }

    #[test]
    fn default_status_is_stable() {
        assert_eq!(OkfStatus::default(), OkfStatus::Stable);
    }

    #[test]
    fn serde_uses_snake_case_for_trust() {
        let json = serde_json::to_string(&OkfTrust::MachineConfirmed).unwrap();
        assert_eq!(json, r#""machine_confirmed""#);
    }

    #[test]
    fn serde_uses_lowercase_for_status() {
        let json = serde_json::to_string(&OkfStatus::Deprecated).unwrap();
        assert_eq!(json, r#""deprecated""#);
    }
}
