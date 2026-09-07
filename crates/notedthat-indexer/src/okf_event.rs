//! `OkfMaintenanceEvent` — the message type on the OKF index-maintenance queue.
//!
//! Events are produced by [`crate::worker::IndexerWorker`], which is the one place
//! that sees every write regardless of which surface made it — HTTP, `WebDAV` or
//! MCP. The consumer lives in `notedthat-write`, which is why the event type is
//! declared here rather than beside its worker: `notedthat-write` depends on this
//! crate, not the other way round.
//!
//! The queue exists at all only when maintenance is switched on. It is
//! **best-effort in a stronger sense than the indexing queue**: on a full queue
//! the event is logged and dropped rather than surfaced as a 503, because
//! maintaining a listing must never fail a user's write.

use notedthat_core::{KbSlug, ObjectPath};

/// A change to a concept that may need reflecting in its directory's listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OkfMaintenanceEvent {
    /// A concept was created or updated.
    Upserted {
        /// Knowledge-base slug for routing.
        kb: KbSlug,
        /// Object key of the concept.
        object_key: ObjectPath,
        /// The concept's OKF `type`, which names the `index.md` section.
        concept_type: String,
        /// Display title for the entry.
        title: String,
        /// Trailing description for the entry, when the concept has one.
        description: Option<String>,
    },
    /// A concept was deleted.
    Deleted {
        /// Knowledge-base slug for routing.
        kb: KbSlug,
        /// Object key of the concept.
        object_key: ObjectPath,
    },
}

impl OkfMaintenanceEvent {
    /// The KB slug, for routing and logging.
    #[must_use]
    pub fn kb(&self) -> &KbSlug {
        match self {
            Self::Upserted { kb, .. } | Self::Deleted { kb, .. } => kb,
        }
    }

    /// The object key, for logging.
    #[must_use]
    pub fn object_key(&self) -> &ObjectPath {
        match self {
            Self::Upserted { object_key, .. } | Self::Deleted { object_key, .. } => object_key,
        }
    }

    /// A static discriminant for structured logging.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Upserted { .. } => "upserted",
            Self::Deleted { .. } => "deleted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    fn path() -> ObjectPath {
        ObjectPath::try_from_str("tables/customers.md").unwrap()
    }

    fn upserted() -> OkfMaintenanceEvent {
        OkfMaintenanceEvent::Upserted {
            kb: kb(),
            object_key: path(),
            concept_type: "BigQuery Table".into(),
            title: "Customers".into(),
            description: Some("Customer master".into()),
        }
    }

    #[test]
    fn accessors_read_through_both_variants() {
        assert_eq!(upserted().kb().as_str(), "notes");
        assert_eq!(upserted().object_key().as_str(), "tables/customers.md");
        let deleted = OkfMaintenanceEvent::Deleted {
            kb: kb(),
            object_key: path(),
        };
        assert_eq!(deleted.kb().as_str(), "notes");
        assert_eq!(deleted.object_key().as_str(), "tables/customers.md");
    }

    #[test]
    fn kind_discriminants() {
        assert_eq!(upserted().kind(), "upserted");
        assert_eq!(
            OkfMaintenanceEvent::Deleted {
                kb: kb(),
                object_key: path()
            }
            .kind(),
            "deleted"
        );
    }

    #[test]
    fn clone_produces_equal() {
        assert_eq!(upserted().clone(), upserted());
    }
}
