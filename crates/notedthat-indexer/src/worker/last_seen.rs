//! The stamp each key was last seen with, bounded.
//!
//! Exists for one reason: on the `fs` backend the watcher reports the server's
//! own writes back to it (D50). The index already ignores that echo for
//! indexable objects by comparing `ETag`s, but an mp3 is never indexed, so
//! there is nothing to compare against — and announcing it twice, once from the
//! write and once from the echo, is exactly what a transcription worker must
//! not see. The worker sees every `Upsert` and `Tombstone` before the echo
//! arrives, so remembering their stamps is enough to recognise it.

use notedthat_core::{KbSlug, ObjectPath};
use std::collections::{HashMap, VecDeque};

/// How many keys are remembered before the oldest is forgotten. Forgetting one
/// costs at most a duplicate announcement for that key, never a missed one.
const CAPACITY: usize = 4096;

type Key = (KbSlug, ObjectPath);

/// Insertion-ordered, bounded map from key to its last stamp: `Some(etag)` for
/// a write, `None` for a deletion.
pub(super) struct LastSeen {
    stamps: HashMap<Key, Option<String>>,
    order: VecDeque<Key>,
    capacity: usize,
}

impl Default for LastSeen {
    fn default() -> Self {
        Self::with_capacity(CAPACITY)
    }
}

impl LastSeen {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            stamps: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub(super) fn record(&mut self, kb: &KbSlug, object_key: &ObjectPath, etag: Option<String>) {
        let key = (kb.clone(), object_key.clone());
        if self.stamps.insert(key.clone(), etag).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.stamps.remove(&oldest);
            }
        }
    }

    /// Whether `etag` is not the stamp the key was last recorded with. A key
    /// never recorded always differs.
    pub(super) fn differs(&self, kb: &KbSlug, object_key: &ObjectPath, etag: Option<&str>) -> bool {
        self.stamps
            .get(&(kb.clone(), object_key.clone()))
            .is_none_or(|seen| seen.as_deref() != etag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    fn key(s: &str) -> ObjectPath {
        ObjectPath::try_from(s).unwrap()
    }

    #[test]
    fn an_unseen_key_is_news_and_a_repeated_stamp_is_not() {
        let mut seen = LastSeen::default();
        assert!(seen.differs(&kb(), &key("a.mp3"), Some("\"e1\"")));
        seen.record(&kb(), &key("a.mp3"), Some("\"e1\"".into()));
        assert!(!seen.differs(&kb(), &key("a.mp3"), Some("\"e1\"")));
        assert!(seen.differs(&kb(), &key("a.mp3"), Some("\"e2\"")));
        assert!(seen.differs(&kb(), &key("a.mp3"), None));
        seen.record(&kb(), &key("a.mp3"), None);
        assert!(!seen.differs(&kb(), &key("a.mp3"), None));
        assert!(seen.differs(&kb(), &key("a.mp3"), Some("\"e1\"")));
    }

    #[test]
    fn the_oldest_key_is_forgotten_first() {
        let mut seen = LastSeen::with_capacity(2);
        seen.record(&kb(), &key("a"), Some("1".into()));
        seen.record(&kb(), &key("b"), Some("1".into()));
        seen.record(&kb(), &key("c"), Some("1".into()));
        assert!(seen.differs(&kb(), &key("a"), Some("1")), "a was evicted");
        assert!(!seen.differs(&kb(), &key("b"), Some("1")));
        assert!(!seen.differs(&kb(), &key("c"), Some("1")));
        // Re-recording a live key does not grow the order queue.
        seen.record(&kb(), &key("b"), Some("2".into()));
        assert_eq!(seen.order.len(), 2);
        assert!(!seen.differs(&kb(), &key("b"), Some("2")));
    }
}
