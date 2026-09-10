//! Comparing a knowledge base's directory against what a search index holds.
//!
//! # Why a comparison rather than a log of changes
//!
//! Watching a tree tells you what changed while you were listening. It cannot tell you
//! what changed while you were not — during a restart, or while the kernel was dropping
//! events it had no room to queue. Worse, the change that is invisible in both cases is
//! the one that matters most: a walk of the tree finds what exists, and a deleted file
//! leaves nothing behind to find.
//!
//! So the repair is not "replay the changes", which is unknowable, but "compare the two
//! sides and report the differences", which is not. The caller supplies what the index
//! holds; this module walks what storage holds; anything that appears in one and not the
//! other, or in both with different `ETag`s, is reported as a change.
//!
//! # Why it is affordable
//!
//! Every object in the knowledge base is examined on every pass, which sounds expensive
//! and is not. [`crate::meta`]'s freshness stamp means an unchanged object costs a stat
//! and a small sidecar read — its content is never opened — and the consumer skips any
//! object whose `ETag` is already the indexed one. Confirming that a large knowledge base
//! is entirely up to date therefore reads none of its content and embeds nothing.

use notedthat_core::{KbSlug, ObjectPath, StorageError, is_internal_path};
use tokio::sync::mpsc;

use crate::storage::FsStorage;

/// An object whose index entry may be out of date.
///
/// Deliberately does not say *how* it is out of date, or even whether the object still
/// exists. By the time a consumer acts the answer can have changed again, so the only
/// safe instruction is "look at this key again" — the consumer re-reads and decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsChange {
    /// Knowledge base the object belongs to.
    pub kb: KbSlug,
    /// Object key that needs re-examining.
    pub key: ObjectPath,
}

/// One object key an index holds, and the `ETag` it was built from.
///
/// The caller's half of the comparison. It is a plain pair rather than the search
/// backend's own type so that this crate keeps knowing nothing about the indexer — a
/// storage adapter that imported the index would invert the dependency the workspace is
/// built around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedEtag {
    /// Object key the index holds chunks for.
    pub key: String,
    /// `ETag` those chunks were built from.
    pub etag: String,
}

/// What one pass found, for the log line that follows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Objects the walk found in storage.
    pub objects_on_disk: usize,
    /// Objects already indexed from exactly these bytes.
    pub unchanged: usize,
    /// Objects new to the index, or indexed from different bytes.
    pub changed: usize,
    /// Keys the index holds that storage no longer has.
    pub orphaned: usize,
}

impl ReconcileReport {
    /// Whether anything at all needs re-examining.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.changed == 0 && self.orphaned == 0
    }
}

/// Compare one knowledge base against `indexed` and report every difference to `sink`.
///
/// `indexed` must be sorted by key, which is what lets this walk both sides in step
/// instead of building a lookup table over the whole knowledge base.
/// `VectorStore::indexed_objects` returns them that way.
///
/// `prefix` narrows the comparison to one subtree. That is how a directory that was
/// renamed or moved away gets cleaned up: nothing is left on disk under the old prefix, so
/// every key the index still holds there is reported as needing a fresh look, and the
/// consumer's re-read finds it gone.
///
/// # Errors
///
/// Returns the walk's [`StorageError`] if the knowledge base cannot be read. A closed
/// `sink` ends the pass early and is not an error: it means the consumer is shutting down.
pub async fn reconcile(
    storage: &FsStorage,
    kb: &KbSlug,
    prefix: Option<&str>,
    indexed: &[IndexedEtag],
    sink: &mpsc::Sender<FsChange>,
) -> Result<ReconcileReport, StorageError> {
    // `.notedthat/` is private (D48) and nothing indexes it, so comparing it would report
    // the manifest as needing work on every single pass — and, since the manifest is an
    // ordinary file inside the knowledge base's directory, the walk does find it. Filtered
    // on both sides, so a stray entry from an older release is still cleaned up.
    let on_disk: Vec<(String, String)> = storage
        .walk_etags(kb, prefix)
        .await?
        .into_iter()
        .filter(|(key, _)| !is_internal_path(key))
        .collect();
    let indexed: Vec<&IndexedEtag> = indexed
        .iter()
        .filter(|entry| prefix.is_none_or(|prefix| entry.key.starts_with(prefix)))
        .collect();

    let mut report = ReconcileReport {
        objects_on_disk: on_disk.len(),
        ..ReconcileReport::default()
    };

    // Both sides are sorted by key, so one pass with two cursors sees every key exactly
    // once and never has to hold a map of the whole knowledge base.
    let mut disk = on_disk.iter();
    let mut index = indexed.into_iter();
    let mut next_disk = disk.next();
    let mut next_index = index.next();

    loop {
        let outcome = match (next_disk, next_index) {
            (None, None) => break,
            (Some((key, etag)), Some(entry)) => match key.as_str().cmp(entry.key.as_str()) {
                std::cmp::Ordering::Equal => {
                    let same = *etag == entry.etag;
                    next_disk = disk.next();
                    next_index = index.next();
                    if same {
                        report.unchanged += 1;
                        continue;
                    }
                    report.changed += 1;
                    key.clone()
                }
                std::cmp::Ordering::Less => {
                    // On disk, unknown to the index.
                    report.changed += 1;
                    let key = key.clone();
                    next_disk = disk.next();
                    key
                }
                std::cmp::Ordering::Greater => {
                    // Indexed, with no file behind it.
                    report.orphaned += 1;
                    let key = entry.key.clone();
                    next_index = index.next();
                    key
                }
            },
            (Some((key, _)), None) => {
                report.changed += 1;
                let key = key.clone();
                next_disk = disk.next();
                key
            }
            (None, Some(entry)) => {
                report.orphaned += 1;
                let key = entry.key.clone();
                next_index = index.next();
                key
            }
        };

        // An indexed key that is not a valid object path cannot be re-read, so reporting it
        // would only produce an event nothing can act on. It also cannot have come from
        // this adapter, which refuses to write one.
        let Ok(key) = ObjectPath::try_from(outcome.as_str()) else {
            tracing::warn!(key = %outcome, "skipping a key that is not a valid object path");
            continue;
        };
        if sink
            .send(FsChange {
                kb: kb.clone(),
                key,
            })
            .await
            .is_err()
        {
            tracing::debug!(
                kb = %kb.as_str(),
                "reconciliation stopped early: nothing is listening any more"
            );
            break;
        }
    }

    Ok(report)
}
