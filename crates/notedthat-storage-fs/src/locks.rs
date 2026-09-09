//! Per-key serialization, which is what makes conditional writes atomic.
//!
//! `If-Match` on a filesystem is read-then-write, and without a lock two concurrent
//! writers can both read the same `ETag`, both find it satisfied, and both write —
//! losing one update. That is the failure SPECIFICATIONS.md §8.1 records against Garage
//! and pre-4.09 `SeaweedFS`, and the one D46's PATCH splice depends on not happening.
//!
//! The guarantee holds within one process. A second server on the same root would
//! reintroduce the race, which is why [`crate::root`] refuses to start one.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tokio::sync::{Mutex, MutexGuard};

/// Number of lock stripes.
///
/// Fixed rather than a map keyed by object: a map needs entry eviction, and a stripe
/// collision only costs latency between two unrelated keys — which for a notes store is
/// not a cost worth managing a map to avoid.
const STRIPES: usize = 512;

/// Striped per-key locks.
#[derive(Debug)]
pub(crate) struct KeyLocks {
    stripes: Vec<Mutex<()>>,
}

/// Held while one key is being read-modified-written.
pub(crate) struct KeyGuard<'a> {
    _first: MutexGuard<'a, ()>,
    _second: Option<MutexGuard<'a, ()>>,
}

impl KeyLocks {
    pub(crate) fn new() -> Self {
        Self {
            stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(),
        }
    }

    fn stripe_of(bucket: &str, key: &str) -> usize {
        let mut hasher = DefaultHasher::new();
        bucket.hash(&mut hasher);
        key.hash(&mut hasher);
        usize::try_from(hasher.finish() % STRIPES as u64).unwrap_or(0)
    }

    /// Lock one key.
    pub(crate) async fn key(&self, bucket: &str, key: &str) -> KeyGuard<'_> {
        KeyGuard {
            _first: self.stripes[Self::stripe_of(bucket, key)].lock().await,
            _second: None,
        }
    }

    /// Lock two keys, for a copy.
    ///
    /// Acquisition is in ascending stripe order, and a single lock when the two keys
    /// share a stripe. Locking "source then destination" would deadlock the moment two
    /// distinct keys hashed to the same stripe — rare, and therefore not something
    /// ordinary testing would ever surface.
    pub(crate) async fn pair(&self, bucket: &str, first: &str, second: &str) -> KeyGuard<'_> {
        let a = Self::stripe_of(bucket, first);
        let b = Self::stripe_of(bucket, second);

        if a == b {
            return KeyGuard {
                _first: self.stripes[a].lock().await,
                _second: None,
            };
        }

        let (low, high) = if a < b { (a, b) } else { (b, a) };
        KeyGuard {
            _first: self.stripes[low].lock().await,
            _second: Some(self.stripes[high].lock().await),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyLocks, STRIPES};
    use std::sync::Arc;

    #[test]
    fn a_key_maps_to_a_stable_stripe() {
        let first = KeyLocks::stripe_of("nt-t-kb", "a.md");
        assert_eq!(first, KeyLocks::stripe_of("nt-t-kb", "a.md"));
        assert!(first < STRIPES);
    }

    /// Two keys sharing a stripe is the case that would deadlock a naive
    /// source-then-destination acquisition, so it is worth constructing deliberately
    /// rather than hoping a random pair collides.
    #[tokio::test]
    async fn a_pair_sharing_a_stripe_does_not_deadlock() {
        let locks = KeyLocks::new();
        let mut collision = None;
        for candidate in 0..20_000u32 {
            let key = format!("k{candidate}.md");
            if key != "a.md" && KeyLocks::stripe_of("b", &key) == KeyLocks::stripe_of("b", "a.md") {
                collision = Some(key);
                break;
            }
        }
        let other = collision.expect("512 stripes over 20k keys must collide");

        let guard = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            locks.pair("b", "a.md", &other),
        )
        .await
        .expect("pair() must not deadlock on a stripe collision");
        drop(guard);
    }

    #[tokio::test]
    async fn a_pair_is_order_independent() {
        let locks = Arc::new(KeyLocks::new());
        let forward = locks.pair("b", "one.md", "two.md").await;
        drop(forward);
        let reverse = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            locks.pair("b", "two.md", "one.md"),
        )
        .await
        .expect("reverse order must acquire the same stripes without deadlock");
        drop(reverse);
    }

    #[tokio::test]
    async fn a_held_key_lock_excludes_a_second_holder() {
        let locks = Arc::new(KeyLocks::new());
        let held = locks.key("b", "a.md").await;

        let contender = Arc::clone(&locks);
        let waiting = tokio::spawn(async move {
            let _guard = contender.key("b", "a.md").await;
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), async {})
                .await
                .is_ok()
        );
        assert!(!waiting.is_finished(), "the second holder must wait");

        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the waiter must proceed once the lock is released")
            .expect("no panic");
    }
}
