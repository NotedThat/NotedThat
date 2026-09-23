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
//! backends (D50, D67). [`walk_etags`] is the storage side for any backend whose listing
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
/// A key that is not a valid [`ObjectPath`] cannot be re-read, so it is dropped from both
/// sides before anything is counted — see [`is_actionable`].
pub fn compare(
    in_storage: impl IntoIterator<Item = (String, Option<String>)>,
    indexed: &[IndexedEtag],
    prefix: Option<&str>,
) -> Reconciliation {
    let in_storage: Vec<(String, Option<String>)> = in_storage
        .into_iter()
        .filter(|(key, _)| !is_internal_path(key))
        .filter(|(key, _)| is_actionable(key))
        .collect();
    debug_assert!(
        in_storage.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "the storage side must be sorted by key"
    );
    let indexed: Vec<&IndexedEtag> = indexed
        .iter()
        .filter(|entry| prefix.is_none_or(|prefix| entry.key.starts_with(prefix)))
        .filter(|entry| is_actionable(&entry.key))
        .collect();
    // The index side is asserted too. Its order is a property of the vector backend's
    // scroll, not of anything in this crate, so it is the side this function can vouch
    // for least — and a misordered one is not a panic or a bad count but data loss: the
    // merge walks past every key after it and reports each as orphaned, which the
    // consumer turns into tombstones. `<=` rather than `<`, since a duplicate key is
    // survivable where a misorder is not.
    debug_assert!(
        indexed.windows(2).all(|pair| pair[0].key <= pair[1].key),
        "the index side must be sorted by key"
    );

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

        if let Ok(key) = ObjectPath::try_from(outcome.as_str()) {
            keys.push(key);
        } else {
            // Both sides were filtered by `is_actionable` above, so this is unreachable.
            // Dropping the key rather than panicking keeps a pass safe if that ever
            // stops being true.
            debug_assert!(false, "an unvalidated key reached the merge: {outcome}");
            tracing::warn!(key = %outcome, "skipping a key that is not a valid object path");
        }
    }

    Reconciliation { report, keys }
}

/// Whether a key either side reported can be acted on at all.
///
/// A key that is not a valid [`ObjectPath`] cannot be re-read, so an entry for it would
/// only produce an event nothing can act on — and, counted, it would keep
/// [`ReconcileReport::is_clean`] false for that knowledge base on every pass from now on,
/// with a `warn!` per pass to match. On `s3` that is not a corner case but ordinary
/// bucket traffic: S3 accepts keys `ObjectPath` refuses, the commonest being the
/// zero-byte directory marker a console's "create folder" button or `aws s3api
/// put-object --key docs/` leaves behind.
///
/// Both sides are filtered, so the counts only ever describe keys a pass can act on.
/// Filtering the index side cannot turn a real key into a phantom orphan: an index entry
/// is written through an [`ObjectPath`] in the first place, so an unactionable one is
/// already beyond this pass's reach.
fn is_actionable(key: &str) -> bool {
    if ObjectPath::try_from(key).is_ok() {
        return true;
    }
    tracing::warn!(key = %key, "skipping a key that is not a valid object path");
    false
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
/// as orphaned, which is the one outcome nothing would repair. A listing that announces
/// more pages but supplies no cursor to reach them, or that hands back the cursor just
/// sent, is that same partial walk arriving quietly, and is an error for the same reason.
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
        match advance(listing.truncated, listing.next_cursor, cursor.as_deref())? {
            Advance::Done => return Ok(out),
            Advance::Next(next) => cursor = Some(next),
        }
    }
}

/// What the walk does after one page.
#[derive(Debug, PartialEq, Eq)]
enum Advance {
    /// That was the last page.
    Done,
    /// Ask for the next page with this cursor.
    Next(String),
}

/// Decide whether a listing has more pages, refusing the shapes that would end the walk
/// early without saying so.
///
/// `truncated == next_cursor.is_some()` is an invariant of `ListResponse`, stated in its
/// doc comment and honoured by every adapter in the workspace. It is not honoured *here*:
/// [`walk_etags`] takes a `&dyn Storage` and is the shared entry point for any backend a
/// later release adds, and the invariant lives in a doc comment rather than in this loop.
/// So a listing that announces more pages but supplies no cursor to reach them is an
/// error rather than a quiet end, and so is one that hands back the cursor just sent —
/// which would otherwise spin the loop forever, growing the walk without bound.
///
/// Both matter because a partial walk does not look partial. Everything it did not reach
/// is reported as orphaned by [`compare`] and tombstoned by the consumer, so a truncated
/// first page over a large bucket would drop that knowledge base's whole index and the
/// next pass would report it clean.
fn advance(
    truncated: bool,
    next_cursor: Option<String>,
    sent: Option<&str>,
) -> Result<Advance, StorageError> {
    match (truncated, next_cursor) {
        (false, _) => Ok(Advance::Done),
        (true, None) => Err(StorageError::BackendUnavailable {
            message: "listing reported more pages but supplied no cursor".to_string(),
        }),
        (true, Some(next)) if Some(next.as_str()) == sent => {
            Err(StorageError::BackendUnavailable {
                message: "listing repeated the cursor it was given".to_string(),
            })
        }
        (true, Some(next)) => Ok(Advance::Next(next)),
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
    fn a_key_that_is_not_an_object_path_is_dropped_before_it_is_counted() {
        let r = compare(
            stored(&[("a.md", "\"1\"")]),
            &indexed(&[("../escape.md", "\"x\"")]),
            None,
        );
        assert_eq!(r.report.changed, 1);
        assert_eq!(
            r.report.orphaned, 0,
            "nothing can re-read it, so counting it would keep every pass dirty"
        );
        assert_eq!(keys(&r), ["a.md"], "and it is never handed to a consumer");
    }

    #[test]
    fn a_directory_marker_in_the_bucket_leaves_the_pass_clean() {
        // `aws s3api put-object --key docs/`, or a console's "create folder" button:
        // a zero-byte key S3 accepts and `ObjectPath` refuses. Counting it would keep
        // `is_clean()` false for this knowledge base on every pass from now on.
        let r = compare(
            vec![
                ("docs/".to_string(), Some("\"d41d8c\"".to_string())),
                ("docs/a.md".to_string(), Some("\"1\"".to_string())),
            ],
            &indexed(&[("docs/a.md", "\"1\"")]),
            None,
        );
        assert_eq!(r.report.objects_on_disk, 1, "the marker is not an object");
        assert_eq!(r.report.unchanged, 1);
        assert_eq!(r.report.changed, 0);
        assert_eq!(r.report.orphaned, 0);
        assert!(r.report.is_clean());
        assert!(keys(&r).is_empty());
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

    #[test]
    fn an_untruncated_page_ends_the_walk() {
        assert_eq!(advance(false, None, None).unwrap(), Advance::Done);
        assert_eq!(
            advance(false, Some("ignored".to_string()), None).unwrap(),
            Advance::Done
        );
    }

    #[test]
    fn a_truncated_page_with_a_cursor_asks_for_the_next_one() {
        assert_eq!(
            advance(true, Some("page-2".to_string()), Some("page-1")).unwrap(),
            Advance::Next("page-2".to_string())
        );
    }

    #[test]
    fn a_truncated_page_without_a_cursor_fails_the_walk() {
        // Ending quietly here would report every unreached key as orphaned, and the
        // consumer would tombstone the lot.
        let error = advance(true, None, None).unwrap_err();
        assert!(
            matches!(&error, StorageError::BackendUnavailable { message }
                if message.contains("supplied no cursor")),
            "{error}"
        );
    }

    #[test]
    fn a_repeated_cursor_fails_the_walk() {
        let error = advance(true, Some("page-1".to_string()), Some("page-1")).unwrap_err();
        assert!(
            matches!(&error, StorageError::BackendUnavailable { message }
                if message.contains("repeated the cursor")),
            "{error}"
        );
    }
}
