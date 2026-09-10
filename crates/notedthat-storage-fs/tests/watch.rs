//! Watching a real tree, and what the kernel does and does not report about it.
//!
//! These are the tests that cannot be written against synthesized events, because what is
//! being pinned is the *kernel's* behaviour as much as ours: which operations produce
//! per-file reports, which produce only a directory, and which produce nothing at all.
//!
//! # No sleeps
//!
//! Every wait is a deadline on a channel receive, so a fast machine finishes fast and a
//! slow one still passes. Asserting that something produces *nothing* needs more care —
//! see [`quiet_about`].
//!
//! Run with: `cargo test -p notedthat-storage-fs --test watch`
#![allow(missing_docs)]

use std::path::PathBuf;
use std::time::Duration;

use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, TenantSlug};
use notedthat_storage_fs::{
    FsConfig, FsSignal, FsStorage, FsWatchConfig, FsWatcher, RootLock, open_root, watch_kbs,
};
use tokio::sync::mpsc;

/// Long enough to collapse an editor's several writes, short enough to keep the suite fast.
const DEBOUNCE: Duration = Duration::from_millis(60);
/// Only ever elapses when something is broken.
const DEADLINE: Duration = Duration::from_secs(10);

struct Env {
    _dir: tempfile::TempDir,
    _lock: RootLock,
    _watcher: FsWatcher,
    storage: FsStorage,
    kb: KbSlug,
    bucket: PathBuf,
    signals: mpsc::Receiver<FsSignal>,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = FsConfig::new(dir.path().to_path_buf());
    let lock = open_root(&config).await.expect("root should be usable");
    let storage = FsStorage::new(&config, lock.root().to_path_buf(), TenantSlug::default());
    let kb = KbSlug::try_new("notes").expect("slug");
    storage.ensure_bucket(&kb).await.expect("bucket");

    let bucket = lock.root().join("nt-default-notes");
    let (tx, signals) = mpsc::channel(256);
    let watcher = watch_kbs(
        &storage,
        std::slice::from_ref(&kb),
        FsWatchConfig { debounce: DEBOUNCE },
        tx,
    )
    .expect("watch");

    Env {
        _dir: dir,
        _lock: lock,
        _watcher: watcher,
        storage,
        kb,
        bucket,
        signals,
    }
}

impl Env {
    fn path(&self, key: &str) -> PathBuf {
        self.bucket.join(key)
    }

    /// Write a file the way anything other than `NotedThat` would.
    fn write_by_hand(&self, key: &str, body: &str) {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, body).expect("write");
    }

    /// Wait for the first signal, failing the test rather than hanging.
    async fn next_signal(&mut self) -> FsSignal {
        tokio::time::timeout(DEADLINE, self.signals.recv())
            .await
            .expect("timed out waiting for a signal")
            .expect("the watcher stopped reporting")
    }

    /// Collect signals until `done` is satisfied by everything seen so far.
    ///
    /// Phrased over the accumulated set rather than one signal at a time because a caller
    /// usually wants "until these two keys are covered", and the two can arrive in either
    /// order or together in one subtree report.
    async fn signals_until(&mut self, done: impl Fn(&[FsSignal]) -> bool) -> Vec<FsSignal> {
        let mut seen = Vec::new();
        while !done(&seen) {
            let signal = self.next_signal().await;
            seen.push(signal);
        }
        // Anything already buffered settled in the same batch, so it counts as "seen".
        while let Ok(signal) = self.signals.try_recv() {
            seen.push(signal);
        }
        seen
    }

    /// Collect signals until everything in `keys` would be looked at.
    async fn signals_covering(&mut self, keys: &[&str]) -> Vec<FsSignal> {
        let keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
        self.signals_until(move |seen| {
            keys.iter()
                .all(|key| seen.iter().any(|signal| mentions(signal, key)))
        })
        .await
    }

    /// Assert that whatever just happened produced no report about `key`.
    ///
    /// Waiting a fixed period to "prove" silence would be both slow and a lie. Instead,
    /// touch a second file afterwards and wait for *that* — its arrival proves the watcher
    /// worked through everything queued before it, and a report for `key` would have
    /// settled no later, since its deadline was set first.
    async fn quiet_about(&mut self, key: &str) {
        self.write_by_hand("zz-sentinel.md", "sentinel");
        let seen = self.signals_covering(&["zz-sentinel.md"]).await;
        let mentions: Vec<&FsSignal> = seen.iter().filter(|signal| mentions(signal, key)).collect();
        assert!(
            mentions.is_empty(),
            "expected nothing about {key}, got {mentions:?} (all: {seen:?})"
        );
    }
}

/// Whether a signal would cause `key` to be looked at.
fn mentions(signal: &FsSignal, key: &str) -> bool {
    match signal {
        FsSignal::Changed { key: reported, .. } => reported.as_str() == key,
        FsSignal::Prefix { prefix, .. } => key.starts_with(prefix.as_str()),
        FsSignal::Kb { .. } => true,
    }
}

fn changed(key: &str) -> FsSignal {
    FsSignal::Changed {
        kb: KbSlug::try_new("notes").expect("slug"),
        key: ObjectPath::try_from(key).expect("valid key"),
    }
}

fn prefix(prefix: &str) -> FsSignal {
    FsSignal::Prefix {
        kb: KbSlug::try_new("notes").expect("slug"),
        prefix: prefix.to_owned(),
    }
}

// ─── Objects ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_file_written_by_hand_is_reported() {
    let mut env = env().await;
    env.write_by_hand("hello.md", "# Hello");

    assert_eq!(env.next_signal().await, changed("hello.md"));
}

/// A deletion is reported as "look at this again", never as a deletion. The consumer
/// re-reads and finds it gone — which is what stops a delayed report from removing entries
/// for an object that has since been re-created.
#[tokio::test]
async fn a_file_deleted_by_hand_is_reported_as_a_change() {
    let mut env = env().await;
    env.write_by_hand("gone.md", "here");
    assert_eq!(env.next_signal().await, changed("gone.md"));

    std::fs::remove_file(env.path("gone.md")).expect("remove");

    assert_eq!(env.next_signal().await, changed("gone.md"));
}

/// An editor writing a temporary file and renaming it over the destination must report the
/// destination.
///
/// The editor's own temporary name is reported too, and deliberately so: there is no way
/// to know every editor's naming scheme, and guessing would risk ignoring a real object.
/// It costs one look at a key that no longer exists, which the consumer resolves to
/// nothing. Only *our* temporary prefix is filtered, because that one we do know.
#[tokio::test]
async fn an_editor_style_write_then_rename_reports_the_destination() {
    let mut env = env().await;
    env.write_by_hand("note.md", "first");
    let _ = env.signals_covering(&["note.md"]).await;

    let temp = env.path("note.md.tmp-editor");
    std::fs::write(&temp, "second").expect("write temp");
    std::fs::rename(&temp, env.path("note.md")).expect("rename");

    let seen = env.signals_covering(&["note.md"]).await;
    assert!(
        seen.iter().any(|signal| mentions(signal, "note.md")),
        "the destination must be reported, got {seen:?}"
    );
}

// ─── Directories ────────────────────────────────────────────────────────────

/// Deleting a directory tree must leave nothing behind in the index.
///
/// Asserted as coverage rather than as an exact set, because either outcome is correct:
/// the kernel does report each file, but the directory's own removal arrives in the same
/// window and asks for the subtree, which subsumes them.
#[tokio::test]
async fn removing_a_subdirectory_covers_every_object_it_held() {
    let mut env = env().await;
    env.write_by_hand("sub/a.md", "alpha");
    env.write_by_hand("sub/b.md", "beta");
    let _ = env.signals_covering(&["sub/a.md", "sub/b.md"]).await;

    std::fs::remove_dir_all(env.path("sub")).expect("remove_dir_all");

    let seen = env.signals_covering(&["sub/a.md", "sub/b.md"]).await;
    assert!(!seen.is_empty());
}

/// The gap the kernel leaves. Moving a populated directory in reports the directory and
/// nothing about the files inside it, so the subtree has to be re-examined as a whole.
#[tokio::test]
async fn moving_a_populated_directory_in_reports_the_subtree() {
    let mut env = env().await;
    let outside = tempfile::tempdir().expect("tempdir");
    let source = outside.path().join("vault");
    std::fs::create_dir_all(&source).expect("create");
    std::fs::write(source.join("one.md"), "one").expect("write");
    std::fs::write(source.join("two.md"), "two").expect("write");

    std::fs::rename(&source, env.path("vault")).expect("move in");

    assert_eq!(env.next_signal().await, prefix("vault/"));
}

/// Renaming a directory reports both ends and neither's contents, so both subtrees have to
/// be re-examined: the new one to index what arrived, the old one to notice what left.
#[tokio::test]
async fn renaming_a_directory_reports_both_subtrees() {
    let mut env = env().await;
    env.write_by_hand("before/a.md", "alpha");
    let _ = env.signals_covering(&["before/a.md"]).await;

    std::fs::rename(env.path("before"), env.path("after")).expect("rename");

    let seen = env
        .signals_until(|seen| seen.contains(&prefix("before/")) && seen.contains(&prefix("after/")))
        .await;
    assert!(!seen.is_empty());
}

// ─── What must stay silent ──────────────────────────────────────────────────

/// The regression that matters most. `notify`'s inotify mask always includes `IN_OPEN`, so
/// without the kind filter, serving a read would count as changing the object — and one
/// `grep -r` over the tree would re-index all of it.
#[tokio::test]
async fn reading_an_object_reports_nothing() {
    let mut env = env().await;
    env.write_by_hand("read-me.md", "content");
    let _ = env.signals_covering(&["read-me.md"]).await;

    env.storage
        .get_object(
            &env.kb,
            &ObjectPath::try_from("read-me.md").expect("key"),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect("read");

    env.quiet_about("read-me.md").await;
}

#[tokio::test]
async fn a_permission_change_reports_nothing() {
    let mut env = env().await;
    env.write_by_hand("chmod-me.md", "content");
    let _ = env.signals_covering(&["chmod-me.md"]).await;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            env.path("chmod-me.md"),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod");
    }

    env.quiet_about("chmod-me.md").await;
}

/// Our own in-flight writes. Every commit this adapter makes renames one of these into
/// place, so reporting them would produce work for a key that never exists.
#[tokio::test]
async fn an_in_flight_temp_file_reports_nothing() {
    let mut env = env().await;
    std::fs::write(env.path(".notedthat-tmp-abc123"), "half-written").expect("write");

    env.quiet_about(".notedthat-tmp-abc123").await;
}

/// The knowledge base is meant to be kept under version control, and one commit rewrites
/// hundreds of files under `.git`.
#[tokio::test]
async fn a_version_control_directory_reports_nothing() {
    let mut env = env().await;
    env.write_by_hand(".git/objects/ab/cdef", "an object");

    env.quiet_about(".git/objects/ab/cdef").await;
}

/// The manifest is an ordinary file inside the knowledge base's directory, but it is
/// private and nothing outside this adapter should react to it.
#[tokio::test]
async fn a_manifest_write_reports_nothing() {
    let mut env = env().await;
    let manifest =
        notedthat_core::KbManifest::new_v1(&TenantSlug::default(), &env.kb, "Notes", 1_700_000_000);
    env.storage
        .write_manifest(&env.kb, &manifest)
        .await
        .expect("write manifest");

    env.quiet_about(".notedthat/manifest.json").await;
}

/// The metadata tree sits above the watched directories, so a sidecar write is out of range
/// structurally rather than by a filter rule.
#[tokio::test]
async fn a_sidecar_write_reports_nothing() {
    let mut env = env().await;
    env.storage
        .put_object(
            &env.kb,
            &ObjectPath::try_from("sidecar-check.md").expect("key"),
            "body".into(),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    // The object itself is reported; only its sidecar must not be.
    let seen = env.signals_covering(&["sidecar-check.md"]).await;
    assert!(
        seen.iter()
            .all(|signal| mentions(signal, "sidecar-check.md")),
        "only the object should be reported, got {seen:?}"
    );
}

/// A write through `NotedThat` is reported like any other change. Recognising it as
/// already-indexed is the consumer's job, and this pins which layer owns that.
#[tokio::test]
async fn a_write_through_the_adapter_is_still_reported() {
    let mut env = env().await;
    env.storage
        .put_object(
            &env.kb,
            &ObjectPath::try_from("via-api.md").expect("key"),
            "body".into(),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    assert_eq!(env.next_signal().await, changed("via-api.md"));
}
