#![allow(missing_docs)]

//! Behaviour of `FsStorage` that only shows up against a real directory tree: the
//! browsable layout, out-of-band edits, and the places a filesystem cannot do what S3
//! does.

use bytes::Bytes;
use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, KbSlug, ObjectPath, Storage, StorageError, TenantSlug,
    compute_etag,
};
use notedthat_storage_fs::{FsConfig, FsStorage, RootLock, open_root};

struct Env {
    _dir: tempfile::TempDir,
    _lock: RootLock,
    storage: FsStorage,
    root: std::path::PathBuf,
    kb: KbSlug,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let config = FsConfig::new(root.clone());
    let lock = open_root(&config).await.expect("root should be usable");
    let storage = FsStorage::new(&config, lock.root().to_path_buf(), TenantSlug::default());
    let kb = KbSlug::try_new("notes").expect("slug");
    storage.ensure_bucket(&kb).await.expect("bucket");
    Env {
        root: lock.root().to_path_buf(),
        _dir: dir,
        _lock: lock,
        storage,
        kb,
    }
}

fn path(key: &str) -> ObjectPath {
    ObjectPath::try_from_str(key).expect("valid key")
}

async fn put(env: &Env, key: &str, body: &str) {
    env.storage
        .put_object(
            &env.kb,
            &path(key),
            Bytes::from(body.to_owned()),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");
}

/// The stamp is read from the staged file *before* the rename, so that it can only ever
/// describe the bytes this write produced. That is sound only because `rename` preserves
/// size, mtime and inode — if it did not, every subsequent read would find the record
/// stale and rehash the object, and the freshness check would be permanently useless
/// rather than merely slow.
#[tokio::test]
async fn the_recorded_stamp_matches_the_committed_file() {
    let env = env().await;
    put(&env, "a.md", "hello").await;

    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            env.root
                .join(".notedthat-meta/nt-default-notes/a.md.ntmeta"),
        )
        .expect("read sidecar"),
    )
    .expect("sidecar is json");
    let committed =
        std::fs::symlink_metadata(env.root.join("nt-default-notes/a.md")).expect("stat");

    assert_eq!(record["size"].as_u64(), Some(committed.len()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(record["ino"].as_u64(), Some(committed.ino()));
        assert_eq!(record["mtime_secs"].as_i64(), Some(committed.mtime()));
        assert_eq!(
            record["mtime_nanos"].as_u64(),
            u32::try_from(committed.mtime_nsec()).ok().map(u64::from)
        );
    }
}

/// The whole premise of this backend: an object's key is its path on disk.
#[tokio::test]
async fn an_object_is_a_file_at_its_key_path() {
    let env = env().await;
    put(&env, "notes/hello.md", "# Hello").await;

    let on_disk = env.root.join("nt-default-notes/notes/hello.md");
    assert_eq!(
        std::fs::read_to_string(&on_disk).expect("the object should be a plain file"),
        "# Hello"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&on_disk)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o644,
            "a store nobody but the server can read defeats the point of the layout"
        );
    }
}

/// The manifest is an ordinary object, as it is on S3 — visible to reads and listings.
#[tokio::test]
async fn the_manifest_is_an_ordinary_object() {
    let env = env().await;
    let manifest = notedthat_core::KbManifest::new_v1(&TenantSlug::default(), &env.kb, "Notes", 0);
    env.storage
        .write_manifest(&env.kb, &manifest)
        .await
        .expect("write manifest");

    let read = env
        .storage
        .read_manifest(&env.kb)
        .await
        .expect("read manifest");
    assert_eq!(read.kb_slug.as_str(), "notes");

    let listed = env
        .storage
        .list_objects(&env.kb, None, 100, None)
        .await
        .expect("list");
    assert!(
        listed
            .objects
            .iter()
            .any(|object| object.key == ".notedthat/manifest.json"),
        "the manifest is a real object on S3 and must be one here too"
    );
}

/// The tree is meant to be edited. An edit made outside the server must produce a
/// correct new `ETag`, not a stale recorded one.
#[tokio::test]
async fn an_edit_made_outside_the_server_is_noticed() {
    let env = env().await;
    put(&env, "hello.md", "first").await;

    let before = env
        .storage
        .head_object(&env.kb, &path("hello.md"), ConditionalHeaders::default())
        .await
        .expect("head");
    assert_eq!(
        before.etag.as_deref(),
        Some(compute_etag(b"first").as_str())
    );

    // Someone opens the file in their editor and saves.
    std::fs::write(
        env.root.join("nt-default-notes/hello.md"),
        b"edited by hand",
    )
    .expect("edit");

    let after = env
        .storage
        .get_object(
            &env.kb,
            &path("hello.md"),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect("get");
    assert_eq!(after.bytes, Bytes::from_static(b"edited by hand"));
    assert_eq!(
        after.meta.etag.as_deref(),
        Some(compute_etag(b"edited by hand").as_str())
    );
    assert_eq!(
        after.meta.content_type.as_deref(),
        Some("text/markdown"),
        "the bytes changed, not the declared type"
    );
}

/// A file dropped into the tree by hand is a first-class object.
#[tokio::test]
async fn a_file_dropped_in_by_hand_is_readable() {
    let env = env().await;
    let dir = env.root.join("nt-default-notes/imported");
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::write(dir.join("note.md"), b"# Imported").expect("write");

    let read = env
        .storage
        .get_object(
            &env.kb,
            &path("imported/note.md"),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect("get");
    assert_eq!(read.bytes, Bytes::from_static(b"# Imported"));
    assert_eq!(read.meta.content_type.as_deref(), Some("text/markdown"));
}

#[tokio::test]
async fn conditional_writes_are_enforced() {
    let env = env().await;
    put(&env, "a.md", "one").await;
    let etag = compute_etag(b"one");

    let stale = env
        .storage
        .put_object(
            &env.kb,
            &path("a.md"),
            Bytes::from_static(b"two"),
            None,
            ConditionalHeaders {
                if_match: Some("\"nope\"".into()),
                ..ConditionalHeaders::default()
            },
        )
        .await;
    assert!(matches!(stale, Err(StorageError::PreconditionFailed)));

    let current = env
        .storage
        .put_object(
            &env.kb,
            &path("a.md"),
            Bytes::from_static(b"two"),
            None,
            ConditionalHeaders {
                if_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await;
    assert!(current.is_ok());

    let clobber = env
        .storage
        .put_object(
            &env.kb,
            &path("a.md"),
            Bytes::from_static(b"three"),
            None,
            ConditionalHeaders {
                if_none_match: Some("*".into()),
                ..ConditionalHeaders::default()
            },
        )
        .await;
    assert!(
        matches!(clobber, Err(StorageError::PreconditionFailed)),
        "If-None-Match: * is create-only"
    );
}

#[tokio::test]
async fn range_reads_report_an_inclusive_content_range() {
    let env = env().await;
    put(&env, "a.md", "0123456789").await;

    let read = env
        .storage
        .get_object(
            &env.kb,
            &path("a.md"),
            Some(vec![notedthat_core::ByteRange::FromStart {
                first: 2,
                last: 5,
            }]),
            ConditionalHeaders::default(),
        )
        .await
        .expect("range read");
    assert_eq!(read.bytes, Bytes::from_static(b"2345"));
    assert_eq!(read.content_range.as_deref(), Some("bytes 2-5/10"));
    assert_eq!(read.meta.size, 4, "size reports the slice served, as on S3");

    let past_eof = env
        .storage
        .get_object(
            &env.kb,
            &path("a.md"),
            Some(vec![notedthat_core::ByteRange::FromStart {
                first: 50,
                last: 60,
            }]),
            ConditionalHeaders::default(),
        )
        .await;
    assert!(matches!(
        past_eof,
        Err(StorageError::RangeNotSatisfiable {
            complete_length: 10
        })
    ));
}

#[tokio::test]
async fn streaming_a_range_yields_the_same_bytes_as_reading_it() {
    let env = env().await;
    let body = "x".repeat(200_000);
    put(&env, "big.md", &body).await;

    let stream = env
        .storage
        .get_object_stream(
            &env.kb,
            &path("big.md"),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect("stream");
    let collected =
        futures::TryStreamExt::try_fold(stream.chunks, Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await
        .expect("collect");

    assert_eq!(collected.len(), body.len());
    assert_eq!(
        stream.meta.etag.as_deref(),
        Some(compute_etag(body.as_bytes()).as_str())
    );
}

#[tokio::test]
async fn deleting_is_idempotent_and_leaves_no_empty_directories() {
    let env = env().await;
    put(&env, "deep/nested/note.md", "x").await;

    env.storage
        .delete_object(
            &env.kb,
            &path("deep/nested/note.md"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("delete");
    env.storage
        .delete_object(
            &env.kb,
            &path("deep/nested/note.md"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("deleting twice is a no-op");

    assert!(
        !env.root.join("nt-default-notes/deep").exists(),
        "emptied directories are pruned"
    );
    assert!(
        env.root.join("nt-default-notes").is_dir(),
        "the bucket stays"
    );
}

/// Pruning is not cosmetic: without it, the name stays occupied by a directory and this
/// PUT would fail here while succeeding on S3.
#[tokio::test]
async fn a_key_can_be_reused_after_its_subtree_is_deleted() {
    let env = env().await;
    put(&env, "a/b", "nested").await;
    env.storage
        .delete_object(&env.kb, &path("a/b"), ConditionalHeaders::default())
        .await
        .expect("delete");

    put(&env, "a", "now a file").await;
    let read = env
        .storage
        .get_object(&env.kb, &path("a"), None, ConditionalHeaders::default())
        .await
        .expect("get");
    assert_eq!(read.bytes, Bytes::from_static(b"now a file"));
}

/// The one semantic S3 has and a filesystem cannot: `a/b` as an object *and* `a/b/c` as
/// another. Pinned rather than papered over.
#[tokio::test]
async fn a_key_that_shadows_a_prefix_is_refused_rather_than_corrupting_the_tree() {
    let env = env().await;
    put(&env, "a/b", "an object").await;

    let nested = env
        .storage
        .put_object(
            &env.kb,
            &path("a/b/c"),
            Bytes::from_static(b"impossible"),
            None,
            ConditionalHeaders::default(),
        )
        .await;
    let Err(StorageError::Other { source }) = nested else {
        panic!("expected the collision to be reported");
    };
    assert!(
        source.to_string().contains("collides with a directory"),
        "the error should say why: {source}"
    );

    // And the object that was already there is untouched.
    let read = env
        .storage
        .get_object(&env.kb, &path("a/b"), None, ConditionalHeaders::default())
        .await
        .expect("get");
    assert_eq!(read.bytes, Bytes::from_static(b"an object"));
}

/// A directory is not an object. `WebDAV` relies on this to tell a resource from a
/// collection.
#[tokio::test]
async fn a_directory_is_not_an_object() {
    let env = env().await;
    put(&env, "folder/note.md", "x").await;

    let head = env
        .storage
        .head_object(&env.kb, &path("folder"), ConditionalHeaders::default())
        .await;
    assert!(matches!(head, Err(StorageError::NotFound { .. })));
}

#[tokio::test]
async fn copy_preserves_the_source_etag_and_honours_both_preconditions() {
    let env = env().await;
    put(&env, "source file.md", "shared bytes").await;
    let source_etag = compute_etag(b"shared bytes");

    let copied = env
        .storage
        .copy_object(
            &env.kb,
            &path("source file.md"),
            &path("copied/文 copy.md"),
            CopyObjectOptions {
                source_if_match: Some(source_etag.clone()),
                destination_if_none_match: Some("*".into()),
                content_type: None,
            },
        )
        .await
        .expect("copy");
    assert_eq!(copied.etag.as_deref(), Some(source_etag.as_str()));

    let over_existing = env
        .storage
        .copy_object(
            &env.kb,
            &path("source file.md"),
            &path("copied/文 copy.md"),
            CopyObjectOptions {
                destination_if_none_match: Some("*".into()),
                ..CopyObjectOptions::default()
            },
        )
        .await;
    assert!(matches!(
        over_existing,
        Err(StorageError::PreconditionFailed)
    ));

    let wrong_source = env
        .storage
        .copy_object(
            &env.kb,
            &path("source file.md"),
            &path("elsewhere.md"),
            CopyObjectOptions {
                source_if_match: Some("\"stale\"".into()),
                ..CopyObjectOptions::default()
            },
        )
        .await;
    assert!(matches!(
        wrong_source,
        Err(StorageError::PreconditionFailed)
    ));

    let missing_source = env
        .storage
        .copy_object(
            &env.kb,
            &path("absent.md"),
            &path("elsewhere.md"),
            CopyObjectOptions::default(),
        )
        .await;
    assert!(matches!(missing_source, Err(StorageError::NotFound { .. })));
}

#[tokio::test]
async fn listing_pages_in_key_order_without_gaps_or_duplicates() {
    let env = env().await;
    for index in 0..25 {
        put(&env, &format!("page/doc-{index:04}.md"), "x").await;
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let response = env
            .storage
            .list_objects(&env.kb, Some("page/"), 10, cursor.as_deref())
            .await
            .expect("list");
        assert_eq!(
            response.truncated,
            response.next_cursor.is_some(),
            "the documented ListResponse invariant"
        );
        seen.extend(response.objects.into_iter().map(|object| object.key));
        pages += 1;
        cursor = response.next_cursor;
        if cursor.is_none() || pages > 10 {
            break;
        }
    }

    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 25);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted, seen,
        "pages must be sorted, gapless and duplicate-free"
    );
}

/// A concurrent delete of the key a cursor names must not break an in-flight listing —
/// `WebDAV` pages through a whole knowledge base in a loop.
#[tokio::test]
async fn a_cursor_survives_deletion_of_the_key_it_names() {
    let env = env().await;
    for name in ["a.md", "b.md", "c.md", "d.md"] {
        put(&env, name, "x").await;
    }

    let first = env
        .storage
        .list_objects(&env.kb, None, 2, None)
        .await
        .expect("first page");
    let cursor = first.next_cursor.expect("more pages");
    let last_key = first.objects.last().expect("a key").key.clone();

    env.storage
        .delete_object(&env.kb, &path(&last_key), ConditionalHeaders::default())
        .await
        .expect("delete the cursor key");

    let second = env
        .storage
        .list_objects(&env.kb, None, 2, Some(&cursor))
        .await
        .expect("the cursor must still resolve");
    let keys: Vec<_> = second
        .objects
        .into_iter()
        .map(|object| object.key)
        .collect();
    assert_eq!(keys, vec!["c.md", "d.md"]);
}

#[tokio::test]
async fn list_entries_omit_etag_and_content_type_as_on_s3() {
    let env = env().await;
    put(&env, "a.md", "x").await;

    let response = env
        .storage
        .list_objects(&env.kb, None, 10, None)
        .await
        .expect("list");
    let entry = response.objects.first().expect("one object");
    assert_eq!(entry.etag, None);
    assert_eq!(entry.content_type, None);
    assert_eq!(entry.size, 1);
    assert!(entry.last_modified.is_some());
}

#[tokio::test]
async fn an_unusable_cursor_is_reported_as_backend_unavailable() {
    let env = env().await;
    put(&env, "a.md", "x").await;

    let response = env
        .storage
        .list_objects(&env.kb, None, 10, Some("not-a-cursor"))
        .await;
    assert!(matches!(
        response,
        Err(StorageError::BackendUnavailable { .. })
    ));
}

#[tokio::test]
async fn ensure_bucket_is_idempotent() {
    let env = env().await;
    env.storage.ensure_bucket(&env.kb).await.expect("again");
    env.storage.ensure_bucket(&env.kb).await.expect("and again");
    assert!(env.root.join("nt-default-notes").is_dir());
}

/// Sidecars live outside the bucket directories, so no key can name one and listings
/// need no exclusion rules.
#[tokio::test]
async fn metadata_never_appears_in_the_object_tree() {
    let env = env().await;
    put(&env, "a.md", "x").await;

    let bucket = env.root.join("nt-default-notes");
    let stray: Vec<_> = std::fs::read_dir(&bucket)
        .expect("read bucket")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(stray, vec!["a.md".to_string()]);

    assert!(
        env.root
            .join(".notedthat-meta/nt-default-notes/a.md.ntmeta")
            .is_file()
    );
}

/// `NOTEDTHAT_FS_DIR_MODE` is documented as the mode for created directories, and the
/// metadata tree is not an exception. It used to be: the sidecar tree's intermediate
/// directories were created with a bare `create_dir_all`, so they took `0o777 & !umask`
/// instead. Under a service account's `0o077` umask they landed at `0o700`, and a backup
/// running as another user could read every object but not descend into
/// `.notedthat-meta/` — a copy with no `ETag` and no content type, taken from the backend
/// chosen for being copyable.
///
/// Asserted as "both trees agree" rather than against a literal, because `mkdir` masks
/// the mode with the process umask and the runner's is not ours to assume. `0o750` is
/// picked so that the two paths genuinely differ under an ordinary umask.
#[cfg(unix)]
#[tokio::test]
async fn the_metadata_tree_takes_the_configured_directory_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = FsConfig::new(dir.path().to_path_buf());
    config.dir_mode = 0o750;
    let lock = open_root(&config).await.expect("root");
    let storage = FsStorage::new(&config, lock.root().to_path_buf(), TenantSlug::default());
    let kb = KbSlug::try_new("notes").expect("slug");
    storage.ensure_bucket(&kb).await.expect("bucket");

    storage
        .put_object(
            &kb,
            &path("deep/nested/note.md"),
            Bytes::from_static(b"body"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let mode_of = |relative: &str| {
        std::fs::metadata(lock.root().join(relative))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    };

    assert_eq!(
        mode_of(".notedthat-meta/nt-default-notes/deep/nested"),
        mode_of("nt-default-notes/deep/nested"),
        "the metadata tree must be created with the same configured mode as the objects"
    );
}

// --- Concurrency ------------------------------------------------------------------
//
// The property this backend is built to offer is that a conditional write is atomic
// where Garage and pre-4.09 SeaweedFS leave it racy, and `locks.rs` is the whole
// mechanism. Its own tests prove the mutexes behave; these prove `FsStorage` actually
// takes them. Nothing here is a `stress_` scenario — they are cheap and must run in CI,
// because the failure they guard against is a silent lost update, not a slow one.

/// The headline guarantee: many writers, one `If-Match`, exactly one winner.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_conditional_writes_have_exactly_one_winner() {
    let env = env().await;
    put(&env, "contended.md", "start").await;
    let expected = env
        .storage
        .head_object(
            &env.kb,
            &path("contended.md"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("head")
        .etag
        .expect("an etag");

    let contenders: Vec<_> = (0..16)
        .map(|writer| {
            let storage = env.storage.clone();
            let kb = env.kb.clone();
            let if_match = expected.clone();
            tokio::spawn(async move {
                storage
                    .put_object(
                        &kb,
                        &path("contended.md"),
                        Bytes::from(format!("written by {writer}")),
                        Some("text/markdown"),
                        ConditionalHeaders {
                            if_match: Some(if_match),
                            ..ConditionalHeaders::default()
                        },
                    )
                    .await
            })
        })
        .collect();

    let mut winners = 0;
    for contender in contenders {
        match contender.await.expect("no panic") {
            Ok(_) => winners += 1,
            Err(StorageError::PreconditionFailed) => {}
            Err(other) => panic!("expected a precondition failure, got {other:?}"),
        }
    }
    assert_eq!(
        winners, 1,
        "read-check-write must be atomic: a second winner is a lost update"
    );
}

/// Whoever wins, the `ETag` on disk must describe the bytes on disk. This is what the
/// stamp being taken before the rename buys: an interleaved rename cannot leave one
/// writer's `ETag` recorded against another writer's file, matching `is_fresh_for` and
/// reading back as fresh forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_writes_leave_an_etag_that_matches_the_bytes() {
    let env = env().await;

    let writers: Vec<_> = (0..16)
        .map(|writer| {
            let storage = env.storage.clone();
            let kb = env.kb.clone();
            tokio::spawn(async move {
                storage
                    .put_object(
                        &kb,
                        &path("racy.md"),
                        Bytes::from(format!("body {writer}")),
                        Some("text/markdown"),
                        ConditionalHeaders::default(),
                    )
                    .await
                    .expect("unconditional writes all succeed");
            })
        })
        .collect();
    for writer in writers {
        writer.await.expect("no panic");
    }

    let on_disk = std::fs::read(env.root.join("nt-default-notes/racy.md")).expect("read");
    let reported = env
        .storage
        .head_object(&env.kb, &path("racy.md"), ConditionalHeaders::default())
        .await
        .expect("head")
        .etag
        .expect("an etag");
    assert_eq!(
        reported,
        compute_etag(&on_disk),
        "the recorded ETag must describe the committed bytes"
    );
}

/// `write_manifest` is a writer like any other and must take the same lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_manifest_writes_leave_a_readable_manifest() {
    let env = env().await;

    let writers: Vec<_> = (0..8)
        .map(|version| {
            let storage = env.storage.clone();
            let kb = env.kb.clone();
            tokio::spawn(async move {
                let manifest = notedthat_core::KbManifest::new_v1(
                    &TenantSlug::default(),
                    &kb,
                    "Notes",
                    i64::from(version),
                );
                storage.write_manifest(&kb, &manifest).await
            })
        })
        .collect();
    for writer in writers {
        writer.await.expect("no panic").expect("manifest write");
    }

    env.storage
        .read_manifest(&env.kb)
        .await
        .expect("the manifest must still parse and validate");

    let on_disk =
        std::fs::read(env.root.join("nt-default-notes/.notedthat/manifest.json")).expect("read");
    let reported = env
        .storage
        .head_object(
            &env.kb,
            &path(".notedthat/manifest.json"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("head")
        .etag
        .expect("an etag");
    assert_eq!(reported, compute_etag(&on_disk));
}

/// A listing must not lose keys because other keys are being deleted underneath it.
/// The deleted keys are interleaved with the surviving ones, so a walk that stopped at
/// the first vanished file would drop survivors that sort after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_listing_keeps_every_surviving_key_while_deletes_run() {
    let env = env().await;
    for index in 0..60 {
        put(&env, &format!("k{index:02}.md"), "body").await;
    }

    let deleter = {
        let storage = env.storage.clone();
        let kb = env.kb.clone();
        tokio::spawn(async move {
            for index in (0..60).step_by(2) {
                storage
                    .delete_object(
                        &kb,
                        &path(&format!("k{index:02}.md")),
                        ConditionalHeaders::default(),
                    )
                    .await
                    .expect("delete");
            }
        })
    };

    let listed = env
        .storage
        .list_objects(&env.kb, Some("k"), 1000, None)
        .await
        .expect("list");
    deleter.await.expect("no panic");

    let keys: Vec<&str> = listed.objects.iter().map(|o| o.key.as_str()).collect();
    for index in (1..60).step_by(2) {
        let survivor = format!("k{index:02}.md");
        assert!(
            keys.contains(&survivor.as_str()),
            "{survivor} was never deleted but is missing from the listing"
        );
    }
}

/// Deleting the last object in a directory prunes it, and that prune is not serialized
/// against a write to a sibling key — different keys, different lock stripes. A write
/// whose parent is pruned out from under it must retry rather than fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writes_survive_a_concurrent_prune_of_their_directory() {
    let env = env().await;

    let churn = |key: &'static str| {
        let storage = env.storage.clone();
        let kb = env.kb.clone();
        tokio::spawn(async move {
            for _ in 0..60 {
                storage
                    .put_object(
                        &kb,
                        &path(key),
                        Bytes::from_static(b"body"),
                        Some("text/markdown"),
                        ConditionalHeaders::default(),
                    )
                    .await
                    .expect("a pruned parent must be recreated, not reported as a failure");
                storage
                    .delete_object(&kb, &path(key), ConditionalHeaders::default())
                    .await
                    .expect("delete");
            }
        })
    };

    let (one, two) = (churn("shared/one.md"), churn("shared/two.md"));
    one.await.expect("no panic");
    two.await.expect("no panic");
}
