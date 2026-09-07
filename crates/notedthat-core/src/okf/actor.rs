//! The OKF actor convention.
//!
//! OKF v0.2 §5 names three actor forms: `<producer>/<version>` for agents,
//! `human:<id>` for people, and `process:<id>` for automation. Actors are stored
//! verbatim — classification is derived, never normalised — because the spec puts
//! no registry behind them and consumers must tolerate anything.

use serde::{Deserialize, Serialize};

/// An OKF actor string, stored exactly as it appeared in the frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Actor(String);

/// Classification of an [`Actor`] under the OKF actor convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorKind {
    /// `human:<id>` with a non-empty `<id>`.
    Human,
    /// `process:<id>` with a non-empty `<id>`.
    Process,
    /// Anything else, including the `<producer>/<version>` agent form.
    Producer,
}

impl Actor {
    /// Wrap a raw actor string.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The actor string as written in the document.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Classify this actor.
    #[must_use]
    pub fn kind(&self) -> ActorKind {
        if let Some(id) = self.0.strip_prefix("human:")
            && !id.is_empty()
        {
            return ActorKind::Human;
        }
        if let Some(id) = self.0.strip_prefix("process:")
            && !id.is_empty()
        {
            return ActorKind::Process;
        }
        ActorKind::Producer
    }

    /// Whether this actor is a person.
    ///
    /// Deliberately strict: the prefix is matched case-sensitively and the id must
    /// be non-empty. Escalating trust on a malformed actor is the wrong direction
    /// to fail, so `Human:alice` and a bare `human:` are both *not* human.
    #[must_use]
    pub fn is_human(&self) -> bool {
        self.kind() == ActorKind::Human
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_actor_recognised() {
        assert!(Actor::new("human:alice").is_human());
        assert_eq!(Actor::new("human:alice").kind(), ActorKind::Human);
    }

    #[test]
    fn empty_human_id_is_not_human() {
        assert!(!Actor::new("human:").is_human());
        assert_eq!(Actor::new("human:").kind(), ActorKind::Producer);
    }

    #[test]
    fn human_prefix_is_case_sensitive() {
        assert!(!Actor::new("Human:alice").is_human());
    }

    #[test]
    fn process_actor_recognised() {
        assert_eq!(
            Actor::new("process:finance-nightly").kind(),
            ActorKind::Process
        );
    }

    #[test]
    fn empty_process_id_is_producer() {
        assert_eq!(Actor::new("process:").kind(), ActorKind::Producer);
    }

    #[test]
    fn producer_form_recognised() {
        assert_eq!(
            Actor::new("reference_agent/gemini-2.5-pro").kind(),
            ActorKind::Producer
        );
    }

    #[test]
    fn empty_actor_is_producer() {
        assert_eq!(Actor::new("").kind(), ActorKind::Producer);
    }

    #[test]
    fn producer_form_containing_human_word_is_not_human() {
        assert!(!Actor::new("humanoid/1.0").is_human());
    }

    #[test]
    fn serde_is_transparent() {
        let json = serde_json::to_string(&Actor::new("human:alice")).unwrap();
        assert_eq!(json, r#""human:alice""#);
        let back: Actor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "human:alice");
    }
}
