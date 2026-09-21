//! Comparing a knowledge base's directory against what a search index holds.
//!
//! The comparison itself — why it is a comparison and not a replay, and why it is
//! affordable — lives in [`notedthat_core::reconcile`] and is shared with the `s3`
//! backend (D50, D66). This module supplies the filesystem's half: [`FsStorage::walk_etags`]
//! reads each object's `ETag` from its sidecar stamp, so an unchanged object costs a stat
//! and a small read and its content is never opened.

use notedthat_core::reconcile::{IndexedEtag, ReconcileReport, Reconciliation, compare};
use notedthat_core::{KbSlug, ObjectPath, StorageError};
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

/// Compare one knowledge base against `indexed` and report every difference to `sink`.
///
/// `indexed` must be sorted by key, which is what lets [`compare`] walk both sides in step
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
/// `sink` ends the pass early and is not an error: it means the consumer is shutting down;
/// the report still counts the whole comparison.
pub async fn reconcile(
    storage: &FsStorage,
    kb: &KbSlug,
    prefix: Option<&str>,
    indexed: &[IndexedEtag],
    sink: &mpsc::Sender<FsChange>,
) -> Result<ReconcileReport, StorageError> {
    let on_disk = storage
        .walk_etags(kb, prefix)
        .await?
        .into_iter()
        .map(|(key, etag)| (key, Some(etag)));
    let Reconciliation { report, keys } = compare(on_disk, indexed, prefix);

    for key in keys {
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
