//! Comparing what a knowledge base's storage holds against what a search index holds.
//!
//! # Why a comparison rather than a log of changes
//!
//! Watching for changes tells you what changed while you were listening. It cannot tell
//! you what changed while you were not — during a restart, while the kernel was dropping
//! events it had no room to queue, or, on an object store, ever: S3 has no change feed
//! `NotedThat` can subscribe to portably. Worse, the change that is invisible in every case
//! is the one that matters most: a walk of storage finds what exists, and a deleted object
//! leaves nothing behind to find.
//!
//! So the repair is not "replay the changes", which is unknowable, but "compare the two
//! sides and report the differences", which is not. The caller supplies what the index
//! holds and what storage holds; anything that appears in one and not the other, or in
//! both with different `ETag`s, is reported as needing a fresh look.
//!
//! # Why it is affordable
//!
//! Every object in the knowledge base is examined on every pass, which sounds expensive
//! and is not. Storage answers with keys and `ETag`s — from a listing on `s3`, from a
//! sidecar stamp on `fs` — so an unchanged object's content is never opened, and the
//! consumer of the report skips any object whose `ETag` is already the indexed one.
//! Confirming that a large knowledge base is entirely up to date therefore reads none of
//! its content and embeds nothing.
//!
//! The comparison itself, [`compare`], is backend-agnostic and shared by both storage
//! backends (D50, D66). [`walk_etags`] is the storage side for any backend whose listing
//! reports an `ETag`; the `fs` backend has a cheaper walk of its own.

use crate::{KbSlug, ObjectPath, Storage, StorageError, is_internal_path};

/// One object key an index holds, and the `ETag` it was built from.
///
/// The index's half of the comparison. It is a plain pair rather than the search
/// backend's own type so that the storage crates keep knowing nothing about the indexer —
/// a storage adapter that imported the index would invert the dependency the workspace is
/// built around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedEtag {
    /// Object key the index holds chunks for.
    pub key: String,
    /// `ETag` those chunks were built from.
    pub etag: String,
}

/// What one pass found, for the log line and the health record that follow it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Objects the walk found in storage — on disk, or in the bucket.
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

/// The outcome of [`compare`]: the counts, and every key that needs a fresh look.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// What the pass found.
    pub report: ReconcileReport,
    /// Changed and orphaned keys, in key order.
    ///
    /// Deliberately not split into "changed" and "gone": by the time a consumer acts the
    /// answer can have changed again, so the only safe instruction is "look at this key
    /// again" — the consumer re-reads and decides.
    pub keys: Vec<ObjectPath>,
}

/// Compare what storage holds against what the index holds.
///
/// Both sides must be sorted by key, byte-lexicographically: that is what lets one pass
/// with two cursors see every key exactly once and never hold a map of the whole
/// knowledge base. `VectorStore::indexed_objects` returns keys that way, and so do S3
/// listings and the `fs` backend's walk.
///
/// `in_storage` carries the `ETag` storage reports for each key, or `None` when the
/// backend's listing does not carry one; a missing stamp never equals the indexed one, so
/// the object counts as changed and is re-read — the safe direction.
///
/// `prefix` narrows the comparison to one subtree: the index side is restricted to keys
/// under it (the caller lists storage under it). That is how a directory that was renamed
/// or moved away gets cleaned up: nothing is left in storage under the old prefix, so
/// every key the index still holds there is reported, and the consumer's re-read finds
/// it gone.
///
/// `.notedthat/` is private (D48) and nothing indexes it, so it is dropped from the
/// storage side — comparing it would report the manifest as needing work on every single
/// pass. It is dropped from the storage side only. Leaving the index side unfiltered is
/// deliberate and is what cleans up after an earlier release: a stray `.notedthat/*` entry
/// then appears on one side and not the other, so it is reported, the consumer re-reads
/// it, and — the manifest being `application/json`, which the indexer rejects —
/// tombstones it. Filtering both sides would hide such an entry from the comparison
/// entirely and leave it in the index forever.
///
/// A key that is not a valid [`ObjectPath`] cannot be re-read, so reporting it would only
/// produce an event nothing can act on; it is logged and skipped.
pub fn compare(
    in_storage: impl IntoIterator<Item = (String, Option<String>)>,
    indexed: &[IndexedEtag],
    prefix: Option<&str>,
) -> Reconciliation {
    let in_storage: Vec<(String, Option<String>)> = in_storage
        .into_iter()
        .filter(|(key, _)| !is_internal_path(key))
        .collect();
    debug_assert!(
        in_storage.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "the storage side must be sorted by key"
    );
    let indexed: Vec<&IndexedEtag> = indexed
        .iter()
        .filter(|entry| prefix.is_none_or(|prefix| entry.key.starts_with(prefix)))
        .collect();

    let mut report = ReconcileReport {
        objects_on_disk: in_storage.len(),
        ..ReconcileReport::default()
    };
    let mut keys = Vec::new();

    let mut storage = in_storage.iter();
    let mut index = indexed.into_iter();
    let mut next_storage = storage.next();
    let mut next_index = index.next();

    loop {
        let outcome = match (next_storage, next_index) {
            (None, None) => break,
            (Some((key, etag)), Some(entry)) => match key.as_str().cmp(entry.key.as_str()) {
                std::cmp::Ordering::Equal => {
                    let same = etag.as_deref() == Some(entry.etag.as_str());
                    next_storage = storage.next();
                    next_index = index.next();
                    if same {
                        report.unchanged += 1;
                        continue;
                    }
                    report.changed += 1;
                    key.clone()
                }
                std::cmp::Ordering::Less => {
                    // In storage, unknown to the index.
                    report.changed += 1;
                    let key = key.clone();
                    next_storage = storage.next();
                    key
                }
                std::cmp::Ordering::Greater => {
                    // Indexed, with no object behind it.
                    report.orphaned += 1;
                    let key = entry.key.clone();
                    next_index = index.next();
                    key
                }
            },
            (Some((key, _)), None) => {
                report.changed += 1;
                let key = key.clone();
                next_storage = storage.next();
                key
            }
            (None, Some(entry)) => {
                report.orphaned += 1;
                let key = entry.key.clone();
                next_index = index.next();
                key
            }
        };

        match ObjectPath::try_from(outcome.as_str()) {
            Ok(key) => keys.push(key),
            Err(_) => {
                tracing::warn!(key = %outcome, "skipping a key that is not a valid object path");
            }
        }
    }

    Reconciliation { report, keys }
}

/// The most keys one listing page asks for: S3's `ListObjectsV2` ceiling.
const WALK_PAGE: u32 = 1000;

/// Every `(key, etag)` a knowledge base holds under `prefix`, in key order, through the
/// backend's own listing.
///
/// Pages through [`Storage::list_objects`] following each `next_cursor`, so a bucket of
/// any size costs one `LIST` per thousand keys and nothing else. The `ETag` is whatever
/// the listing reports — `None` on a backend whose listing carries none, which
/// [`compare`] treats as "changed".
///
/// # Errors
///
/// The first listing error ends the walk: a partial walk would report every unlisted key
/// as orphaned, which is the one outcome nothing would repair.
pub async fn walk_etags(
    storage: &dyn Storage,
    kb: &KbSlug,
    prefix: Option<&str>,
) -> Result<Vec<(String, Option<String>)>, StorageError> {
    walk_etags_paged(storage, kb, prefix, WALK_PAGE).await
}

/// [`walk_etags`] with a caller-chosen page size, so a test can prove the cursor is
/// followed without a thousand objects.
pub async fn walk_etags_paged(
    storage: &dyn Storage,
    kb: &KbSlug,
    prefix: Option<&str>,
    page: u32,
) -> Result<Vec<(String, Option<String>)>, StorageError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let listing = storage
            .list_objects(kb, prefix, page, cursor.as_deref())
            .await?;
        out.extend(
            listing
                .objects
                .into_iter()
                .map(|object| (object.key, object.etag)),
        );
        match listing.next_cursor {
            Some(next) if listing.truncated => cursor = Some(next),
            _ => return Ok(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConditionalHeaders;
    use crate::testing::InMemoryStorage;
    use bytes::Bytes;

    fn indexed(entries: &[(&str, &str)]) -> Vec<IndexedEtag> {
        entries
            .iter()
            .map(|(key, etag)| IndexedEtag {
                key: (*key).to_string(),
                etag: (*etag).to_string(),
            })
            .collect()
    }

    fn stored(entries: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
        entries
            .iter()
            .map(|(key, etag)| ((*key).to_string(), Some((*etag).to_string())))
            .collect()
    }

    fn keys(reconciliation: &Reconciliation) -> Vec<&str> {
        reconciliation.keys.iter().map(ObjectPath::as_str).collect()
    }

    #[test]
    fn a_fully_indexed_store_is_clean() {
        let r = compare(
            stored(&[("a.md", "\"1\""), ("b.md", "\"2\"")]),
            &indexed(&[("a.md", "\"1\""), ("b.md", "\"2\"")]),
            None,
        );
        assert!(r.report.is_clean());
        assert_eq!(
            r.report,
            ReconcileReport {
                objects_on_disk: 2,
                unchanged: 2,
                changed: 0,
                orphaned: 0
            }
        );
        assert!(r.keys.is_empty());
    }

    #[test]
    fn new_changed_and_orphaned_keys_are_reported_once_each_in_key_order() {
        let r = compare(
            stored(&[("a.md", "\"1\""), ("c.md", "\"new\""), ("d.md", "\"3\"")]),
            &indexed(&[("a.md", "\"1\""), ("b.md", "\"gone\""), ("d.md", "\"old\"")]),
            None,
        );
        assert_eq!(
            r.report,
            ReconcileReport {
                objects_on_disk: 3,
                unchanged: 1,
                changed: 2,
                orphaned: 1
            }
        );
        assert_eq!(keys(&r), ["b.md", "c.md", "d.md"]);
    }

    #[test]
    fn an_empty_store_orphans_every_indexed_key() {
        let r = compare(
            Vec::new(),
            &indexed(&[("a.md", "\"1\""), ("b.md", "\"2\"")]),
            None,
        );
        assert_eq!(r.report.orphaned, 2);
        assert_eq!(r.report.objects_on_disk, 0);
        assert_eq!(keys(&r), ["a.md", "b.md"]);
    }

    #[test]
    fn a_prefix_narrows_the_index_side() {
        // Storage is already listed under the prefix; the index still holds keys
        // outside it, which must not be reported as orphans of this pass.
        let r = compare(
            stored(&[("docs/a.md", "\"1\"")]),
            &indexed(&[
                ("docs/a.md", "\"1\""),
                ("docs/b.md", "\"2\""),
                ("other/z.md", "\"9\""),
            ]),
            Some("docs/"),
        );
        assert_eq!(r.report.unchanged, 1);
        assert_eq!(r.report.orphaned, 1);
        assert_eq!(keys(&r), ["docs/b.md"]);
    }

    #[test]
    fn the_private_prefix_is_dropped_from_storage_but_not_from_the_index() {
        let r = compare(
            stored(&[(".notedthat/manifest.json", "\"m\""), ("a.md", "\"1\"")]),
            &indexed(&[(".notedthat/manifest.json", "\"m\""), ("a.md", "\"1\"")]),
            None,
        );
        assert_eq!(r.report.objects_on_disk, 1, "the manifest is not an object");
        assert_eq!(
            r.report.orphaned, 1,
            "a stray index entry is reported so it gets tombstoned"
        );
        assert_eq!(keys(&r), [".notedthat/manifest.json"]);
    }

    #[test]
    fn a_listing_without_an_etag_counts_as_changed() {
        let r = compare(
            vec![("a.md".to_string(), None)],
            &indexed(&[("a.md", "\"1\"")]),
            None,
        );
        assert_eq!(r.report.changed, 1);
        assert_eq!(keys(&r), ["a.md"]);
    }

    #[test]
    fn a_key_that_is_not_an_object_path_is_skipped() {
        let r = compare(
            stored(&[("a.md", "\"1\"")]),
            &indexed(&[("../escape.md", "\"x\"")]),
            None,
        );
        assert_eq!(r.report.changed, 1);
        assert_eq!(r.report.orphaned, 1, "counted, since it is in the index");
        assert_eq!(keys(&r), ["a.md"], "but never handed to a consumer");
    }

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    async fn seeded(keys: &[&str]) -> InMemoryStorage {
        let storage = InMemoryStorage::with_kbs([&kb()]);
        for key in keys {
            storage
                .put_object(
                    &kb(),
                    &ObjectPath::try_from(*key).unwrap(),
                    Bytes::from(format!("body of {key}")),
                    Some("text/markdown"),
                    ConditionalHeaders::default(),
                )
                .await
                .unwrap();
        }
        storage
    }

    #[tokio::test]
    async fn the_walk_follows_the_cursor_and_reports_each_etag() {
        let storage = seeded(&["a.md", "b.md", "c.md", "d.md", "e.md"]).await;
        let walked = walk_etags_paged(&storage, &kb(), None, 2).await.unwrap();
        let keys: Vec<&str> = walked.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["a.md", "b.md", "c.md", "d.md", "e.md"]);
        for (key, etag) in &walked {
            let head = storage
                .head_object(
                    &kb(),
                    &ObjectPath::try_from(key.as_str()).unwrap(),
                    ConditionalHeaders::default(),
                )
                .await
                .unwrap();
            assert_eq!(etag.as_deref(), head.etag.as_deref(), "{key}");
        }
    }

    #[tokio::test]
    async fn the_walk_honours_the_prefix() {
        let storage = seeded(&["docs/a.md", "docs/b.md", "other/z.md"]).await;
        let walked = walk_etags_paged(&storage, &kb(), Some("docs/"), 1)
            .await
            .unwrap();
        let keys: Vec<&str> = walked.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["docs/a.md", "docs/b.md"]);
    }

    #[tokio::test]
    async fn the_walk_reports_a_missing_bucket() {
        let storage = InMemoryStorage::default();
        let error = walk_etags(&storage, &kb(), None).await.unwrap_err();
        assert!(
            matches!(error, StorageError::BucketNotFound { .. }),
            "{error}"
        );
    }
}
