//! Comparing a knowledge base's directory against what an index holds.
//!
//! The subject is [`notedthat_storage_fs::reconcile`], driven over a real tree in a
//! temporary directory. The index side is a plain list, because that is exactly what the
//! function takes — keeping the search backend out of these tests is the point of the
//! `IndexedEtag` seam.
//!
//! Run with: `cargo test -p notedthat-storage-fs --test reconcile`
#![allow(missing_docs)]

use notedthat_core::{KbSlug, ObjectPath, Storage, TenantSlug};
use notedthat_storage_fs::{
    FsChange, FsConfig, FsStorage, IndexedEtag, RootLock, open_root, reconcile,
};
use tokio::sync::mpsc;

struct Env {
    _dir: tempfile::TempDir,
    _lock: RootLock,
    storage: FsStorage,
    kb: KbSlug,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = FsConfig::new(dir.path().to_path_buf());
    let lock = open_root(&config).await.expect("root should be usable");
    let storage = FsStorage::new(&config, lock.root().to_path_buf(), TenantSlug::default());
    let kb = KbSlug::try_new("notes").expect("slug");
    storage.ensure_bucket(&kb).await.expect("bucket");
    Env {
        _dir: dir,
        _lock: lock,
        storage,
        kb,
    }
}

impl Env {
    async fn put(&self, key: &str, body: &str) -> String {
        let outcome = self
            .storage
            .put_object(
                &self.kb,
                &opath(key),
                body.to_owned().into(),
                Some("text/markdown"),
                notedthat_core::ConditionalHeaders::default(),
            )
            .await
            .expect("put");
        outcome.etag.expect("the adapter always reports an ETag")
    }

    /// Run one pass and collect the keys it asked to have re-examined, in order.
    async fn run(&self, prefix: Option<&str>, indexed: &[IndexedEtag]) -> (Vec<String>, Report) {
        let (tx, mut rx) = mpsc::channel(64);
        let report = reconcile(&self.storage, &self.kb, prefix, indexed, &tx)
            .await
            .expect("reconcile");
        drop(tx);

        let mut keys = Vec::new();
        while let Some(FsChange { kb, key }) = rx.recv().await {
            assert_eq!(
                kb, self.kb,
                "every change names the knowledge base it is in"
            );
            keys.push(key.as_str().to_owned());
        }
        (
            keys,
            Report {
                on_disk: report.objects_on_disk,
                unchanged: report.unchanged,
                changed: report.changed,
                orphaned: report.orphaned,
                clean: report.is_clean(),
            },
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Report {
    on_disk: usize,
    unchanged: usize,
    changed: usize,
    orphaned: usize,
    clean: bool,
}

fn opath(key: &str) -> ObjectPath {
    ObjectPath::try_from(key).expect("valid key")
}

fn indexed(entries: &[(&str, &str)]) -> Vec<IndexedEtag> {
    let mut entries: Vec<IndexedEtag> = entries
        .iter()
        .map(|(key, etag)| IndexedEtag {
            key: (*key).to_owned(),
            etag: (*etag).to_owned(),
        })
        .collect();
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

/// The case that has to be free, because it is what every boot and every dropped-event
/// recovery does: a knowledge base that is already fully indexed reports nothing at all.
#[tokio::test]
async fn a_fully_indexed_tree_reports_no_changes() {
    let env = env().await;
    let a = env.put("a.md", "alpha").await;
    let b = env.put("notes/b.md", "beta").await;

    let (keys, report) = env
        .run(None, &indexed(&[("a.md", &a), ("notes/b.md", &b)]))
        .await;

    assert!(
        keys.is_empty(),
        "nothing should need re-examining, got {keys:?}"
    );
    assert_eq!(
        report,
        Report {
            on_disk: 2,
            unchanged: 2,
            changed: 0,
            orphaned: 0,
            clean: true,
        }
    );
}

#[tokio::test]
async fn an_object_the_index_has_never_seen_is_reported() {
    let env = env().await;
    let a = env.put("a.md", "alpha").await;
    env.put("new.md", "brand new").await;

    let (keys, report) = env.run(None, &indexed(&[("a.md", &a)])).await;

    assert_eq!(keys, vec!["new.md"]);
    assert_eq!(report.changed, 1);
    assert_eq!(report.unchanged, 1);
    assert_eq!(report.orphaned, 0);
}

#[tokio::test]
async fn an_object_indexed_from_different_bytes_is_reported() {
    let env = env().await;
    env.put("a.md", "alpha").await;

    let (keys, report) = env
        .run(None, &indexed(&[("a.md", "\"an-older-etag\"")]))
        .await;

    assert_eq!(keys, vec!["a.md"]);
    assert_eq!(report.changed, 1);
    assert_eq!(report.unchanged, 0);
}

/// The half a walk cannot find on its own. An object deleted while nothing was watching
/// leaves no trace in the tree, so the only evidence it ever existed is the index entry
/// with no file behind it.
#[tokio::test]
async fn a_key_the_index_holds_with_no_file_behind_it_is_reported() {
    let env = env().await;
    let a = env.put("a.md", "alpha").await;

    let (keys, report) = env
        .run(
            None,
            &indexed(&[("a.md", &a), ("deleted-while-away.md", "\"whatever\"")]),
        )
        .await;

    assert_eq!(keys, vec!["deleted-while-away.md"]);
    assert_eq!(report.orphaned, 1);
    assert_eq!(report.unchanged, 1);
    assert!(!report.clean);
}

/// Both sides are walked in step, so a mixture has to come out in key order with each key
/// visited exactly once — a merge that lost its place would double-report or skip.
#[tokio::test]
async fn a_mixture_is_reported_once_per_key_in_key_order() {
    let env = env().await;
    let keep = env.put("b-unchanged.md", "same").await;
    env.put("a-new.md", "new").await;
    env.put("c-modified.md", "modified").await;

    let (keys, report) = env
        .run(
            None,
            &indexed(&[
                ("b-unchanged.md", &keep),
                ("c-modified.md", "\"stale\""),
                ("d-orphaned.md", "\"gone\""),
            ]),
        )
        .await;

    assert_eq!(keys, vec!["a-new.md", "c-modified.md", "d-orphaned.md"]);
    assert_eq!(
        report,
        Report {
            on_disk: 3,
            unchanged: 1,
            changed: 2,
            orphaned: 1,
            clean: false,
        }
    );
}

/// How a renamed or moved-away directory gets cleaned up: nothing remains on disk under
/// the old prefix, so every key the index still holds there is reported, and nothing
/// outside the prefix is touched.
#[tokio::test]
async fn a_prefix_narrows_both_sides() {
    let env = env().await;
    let outside = env.put("elsewhere.md", "untouched").await;
    let inside = env.put("kept/still-here.md", "here").await;

    let (keys, report) = env
        .run(
            Some("kept/"),
            &indexed(&[
                ("elsewhere.md", &outside),
                ("kept/still-here.md", &inside),
                ("kept/moved-away.md", "\"gone\""),
            ]),
        )
        .await;

    assert_eq!(
        keys,
        vec!["kept/moved-away.md"],
        "only the prefix is compared, and only its orphan is reported"
    );
    assert_eq!(report.on_disk, 1);
    assert_eq!(report.orphaned, 1);
    assert_eq!(report.unchanged, 1);
}

/// An empty knowledge base with an index full of keys is the whole-tree-deleted case.
#[tokio::test]
async fn an_empty_tree_reports_every_indexed_key() {
    let env = env().await;

    let (keys, report) = env
        .run(None, &indexed(&[("a.md", "\"x\""), ("b.md", "\"y\"")]))
        .await;

    assert_eq!(keys, vec!["a.md", "b.md"]);
    assert_eq!(report.orphaned, 2);
    assert_eq!(report.on_disk, 0);
}

/// The manifest is an ordinary file inside the knowledge base's directory, so a walk finds
/// it — but it is private (D48) and nothing indexes it. Left in, it would be reported as
/// needing work on every pass, and every startup would tombstone something that was never
/// indexed.
#[tokio::test]
async fn the_private_directory_is_not_compared() {
    let env = env().await;
    let manifest =
        notedthat_core::KbManifest::new_v1(&TenantSlug::default(), &env.kb, "Notes", 1_700_000_000);
    env.storage
        .write_manifest(&env.kb, &manifest)
        .await
        .expect("write manifest");
    let a = env.put("a.md", "alpha").await;

    let (keys, report) = env.run(None, &indexed(&[("a.md", &a)])).await;

    assert!(
        keys.is_empty(),
        "nothing should need re-examining, got {keys:?}"
    );
    assert_eq!(
        report.on_disk, 1,
        "the manifest is not one of the objects compared"
    );
    assert!(report.clean);
}
