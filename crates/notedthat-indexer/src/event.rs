//! `IndexEvent` — the message type on the async indexing queue.
//!
//! Producer (in `notedthat-api-http`'s `commit()` and DELETE handler) enqueues events
//! via `mpsc::Sender::try_send`. Consumer (`IndexerWorker` in this crate) drains via
//! `mpsc::Receiver::recv`. See §6.12 (Indexing queue) and D38 for semantics.

use notedthat_core::{KbSlug, ObjectPath};

/// A single indexing side-effect enqueued after a successful storage operation.
///
/// Best-effort per D38: producers use `try_send`; on queue-full the event is
/// dropped with an `INDEX_QUEUE_FULL` log and the write still succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEvent {
    /// Object was written to S3; re-read and upsert into Qdrant.
    Upsert {
        /// Knowledge-base slug for routing.
        kb: KbSlug,
        /// Object key in the KB bucket.
        object_key: ObjectPath,
        /// S3 `ETag` for cache validation.
        etag: String,
        /// Modification time (Unix timestamp).
        mtime: i64,
    },
    /// Object was deleted from S3; delete matching points from Qdrant.
    Tombstone {
        /// Knowledge-base slug for routing.
        kb: KbSlug,
        /// Object key in the KB bucket.
        object_key: ObjectPath,
    },
}

impl IndexEvent {
    /// Returns the KB slug for logging and routing.
    pub fn kb(&self) -> &KbSlug {
        match self {
            IndexEvent::Upsert { kb, .. } | IndexEvent::Tombstone { kb, .. } => kb,
        }
    }

    /// Returns the object key for logging.
    pub fn object_key(&self) -> &ObjectPath {
        match self {
            IndexEvent::Upsert { object_key, .. } | IndexEvent::Tombstone { object_key, .. } => {
                object_key
            }
        }
    }

    /// Returns a static string discriminant for structured logging.
    pub fn kind(&self) -> &'static str {
        match self {
            IndexEvent::Upsert { .. } => "upsert",
            IndexEvent::Tombstone { .. } => "tombstone",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_kb() -> KbSlug {
        KbSlug::try_new("test-kb").expect("valid slug")
    }

    fn make_path() -> ObjectPath {
        ObjectPath::try_from("hello.md").expect("valid path")
    }

    #[test]
    fn upsert_kb_returns_kb() {
        let ev = IndexEvent::Upsert {
            kb: make_kb(),
            object_key: make_path(),
            etag: "abc".into(),
            mtime: 0,
        };
        assert_eq!(ev.kb().as_str(), "test-kb");
    }

    #[test]
    fn tombstone_kb_returns_kb() {
        let ev = IndexEvent::Tombstone {
            kb: make_kb(),
            object_key: make_path(),
        };
        assert_eq!(ev.kb().as_str(), "test-kb");
    }

    #[test]
    fn object_key_returns_path() {
        let ev = IndexEvent::Upsert {
            kb: make_kb(),
            object_key: make_path(),
            etag: "e".into(),
            mtime: 1,
        };
        assert_eq!(ev.object_key().as_str(), "hello.md");
    }

    #[test]
    fn kind_discriminant() {
        let upsert = IndexEvent::Upsert {
            kb: make_kb(),
            object_key: make_path(),
            etag: "e".into(),
            mtime: 0,
        };
        let tomb = IndexEvent::Tombstone {
            kb: make_kb(),
            object_key: make_path(),
        };
        assert_eq!(upsert.kind(), "upsert");
        assert_eq!(tomb.kind(), "tombstone");
    }

    #[test]
    fn clone_produces_equal() {
        let ev = IndexEvent::Upsert {
            kb: make_kb(),
            object_key: make_path(),
            etag: "e".into(),
            mtime: 42,
        };
        assert_eq!(ev.clone(), ev);
    }

    // Compile-only bounds check
    fn _channel_bounds()
    where
        IndexEvent: Send + Sync + Clone + std::fmt::Debug + 'static,
    {
    }
}
