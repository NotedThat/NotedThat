//! Noticing that the tree changed, without being told.
//!
//! # What this is for
//!
//! The store is meant to be browsable: opened in an editor, `grep`ed, `rsync`ed, kept
//! under version control. Every one of those changes objects without going through
//! `NotedThat`, and [`crate::meta`]'s freshness stamp already makes reads correct
//! afterwards. What it cannot fix is a *search index*, which has no reason to look at an
//! object again unless something says it changed.
//!
//! This is that something. It watches each declared knowledge base's directory and reports
//! what needs re-examining — one object, one subtree, or a whole knowledge base.
//!
//! # What it deliberately does not do
//!
//! It never decides *what* happened, only that something did. Nothing here distinguishes a
//! write from a delete, and [`FsSignal::Changed`] does not say whether the object still
//! exists. The consumer re-reads and finds out.
//!
//! That is not squeamishness, it is the one design choice that makes this safe. Reports
//! are held briefly before being emitted, so a report derived from an event could arrive
//! after the world moved on — and a *deletion* derived that way, landing after the object
//! was legitimately re-created, would remove live index entries with nothing left to put
//! them back. Reporting only "look at this again" makes that unrepresentable.
//!
//! # Which events count
//!
//! The filter is a reject-list, not an accept-list, because the two mistakes are not
//! equally bad: a needless report costs one lookup that finds nothing changed, while a
//! missed one leaves a document wrong in search indefinitely.
//!
//! Two kinds are rejected, and the first one matters enormously. `notify`'s inotify watch
//! mask is fixed and includes `IN_OPEN` and `IN_CLOSE_NOWRITE`, so **every read of every
//! object raises an event**. Without that rejection, serving a `GET` — or one `grep -r`
//! over the tree — would re-examine the entire knowledge base. The second is metadata-only
//! change: a `chmod` moves no bytes.
//!
//! # Which paths count
//!
//! Each knowledge base's own directory is watched, rather than the storage root. The
//! metadata tree and the lock file sit above those directories, so they are out of range
//! structurally and need no filter rule at all — the same invariant [`crate::layout`]
//! documents for listings. Each watch carries its [`KbSlug`], so a directory name is never
//! parsed back into a tenant and knowledge base; that mapping is not reversible.
//!
//! # Directories
//!
//! A directory needs a walk rather than a single look, because the kernel reports the
//! directory and not its contents. Moving a populated directory in reports only the move;
//! a directory created and filled quickly can have files land before the kernel starts
//! watching it; and renaming a directory reports both ends but none of the files under
//! either. All three are answered the same way — re-examine that subtree — and
//! [`pending`] collapses a burst of them into the smallest set of walks that covers them.

mod pending;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notedthat_core::{Error, KbSlug, ObjectPath, is_internal_path};
use notify::{Config, Event, EventKind, RecursiveMode, Watcher, event::ModifyKind};
use tokio::sync::mpsc;

use crate::layout::{TEMP_PREFIX, key_from_path};
use crate::storage::FsStorage;

/// Directory names whose contents are never objects anyone asked `NotedThat` to hold.
///
/// The documentation recommends keeping a knowledge base under version control, and a
/// single `git commit` rewrites hundreds of files inside `.git`. Left in, every commit
/// would re-examine them all.
const VCS_DIRS: [&str; 3] = [".git", ".svn", ".hg"];

/// How much pending work is held before trading the detail for a full pass.
const PENDING_CAPACITY: usize = 65_536;

/// What the watcher reports.
///
/// Three widths of the same instruction — look at this again — because the kernel
/// sometimes names an object, sometimes only the directory above it, and sometimes admits
/// it lost track entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsSignal {
    /// One object may have changed.
    Changed {
        /// Knowledge base the object belongs to.
        kb: KbSlug,
        /// Object key to look at again.
        key: ObjectPath,
    },
    /// Everything under a key prefix may have changed. The prefix ends in `/`.
    Prefix {
        /// Knowledge base the subtree belongs to.
        kb: KbSlug,
        /// Key prefix to re-examine, ending in `/`.
        prefix: String,
    },
    /// A whole knowledge base may have changed.
    ///
    /// Reported when the kernel drops events, when a watch is lost, and when so much
    /// changed at once that tracking it individually was no longer worth it.
    Kb {
        /// Knowledge base to re-examine.
        kb: KbSlug,
    },
}

/// How the watcher behaves.
#[derive(Debug, Clone, Copy)]
pub struct FsWatchConfig {
    /// How long a path must go unreported before it is emitted.
    ///
    /// Long enough to collapse an editor's several writes into one look, short enough that
    /// a save feels searchable straight away.
    pub debounce: Duration,
}

impl Default for FsWatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(500),
        }
    }
}

/// One watched knowledge base, and the directory its objects live in.
#[derive(Debug, Clone)]
struct Watched {
    kb: KbSlug,
    dir: PathBuf,
}

/// A live watch over the declared knowledge bases' directories.
#[must_use = "dropping the FsWatcher stops watching the tree"]
pub struct FsWatcher {
    /// Held for its lifetime: dropping it ends the kernel subscription.
    watcher: notify::RecommendedWatcher,
    settler: tokio::task::JoinHandle<()>,
}

impl FsWatcher {
    /// Stop watching, and wait until nothing more can be reported.
    ///
    /// Worth awaiting rather than dropping at shutdown: a consumer draining its queue
    /// should not be racing a producer still filling it.
    pub async fn stop(self) {
        drop(self.watcher);
        self.settler.abort();
        let _ = self.settler.await;
    }
}

/// Watch each of `kbs`' directories and report what needs re-examining.
///
/// # Errors
///
/// Returns [`Error::Config`] when a watch cannot be established. On Linux that is nearly
/// always the per-user watch limit, so the diagnostic names it alongside the directory
/// count that was needed and the setting that turns watching off — a knowledge base too
/// large to watch should not be a knowledge base that cannot be served.
pub fn watch_kbs(
    storage: &FsStorage,
    kbs: &[KbSlug],
    config: FsWatchConfig,
    signals: mpsc::Sender<FsSignal>,
) -> Result<FsWatcher, Error> {
    let targets: Vec<Watched> = kbs
        .iter()
        .map(|kb| Watched {
            kb: kb.clone(),
            dir: storage.bucket_dir(kb),
        })
        .collect();

    let mut directories = HashSet::new();
    for entry in &targets {
        collect_directories(&entry.dir, &mut directories);
    }

    let pending = Arc::new(Mutex::new(pending::Pending::new(PENDING_CAPACITY)));
    let handler_pending = Arc::clone(&pending);
    let handler_targets = targets.clone();

    let mut watcher = notify::RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            let mut pending = handler_pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match result {
                Ok(event) => handle_event(
                    &event,
                    &handler_targets,
                    &mut directories,
                    &mut pending,
                    Instant::now(),
                ),
                Err(error) => handle_error(&error, &handler_targets, &mut pending, Instant::now()),
            }
        },
        // Left on, `notify` walks out of the storage root and watches whatever a link
        // points at. The adapter already refuses to serve a symlink as an object, so
        // following one here would only spend watches on files nothing can index.
        Config::default().with_follow_symlinks(false),
    )
    .map_err(|error| watch_error(&error, directory_count(&targets)))?;

    for entry in &targets {
        watcher
            .watch(&entry.dir, RecursiveMode::Recursive)
            .map_err(|error| watch_error(&error, directory_count(&targets)))?;
    }

    let settler = tokio::spawn(settle_loop(pending, signals, config.debounce));

    Ok(FsWatcher { watcher, settler })
}

/// Emit work once it has stopped changing.
///
/// Ticks rather than sleeping for the whole window so that a path reported repeatedly is
/// emitted promptly once it settles, instead of on a fixed cadence.
async fn settle_loop(
    pending: Arc<Mutex<pending::Pending>>,
    signals: mpsc::Sender<FsSignal>,
    debounce: Duration,
) {
    let tick = (debounce / 4).max(Duration::from_millis(10));
    loop {
        tokio::time::sleep(tick).await;

        let (settled, overflowed) = {
            let mut pending = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                pending.take_settled(Instant::now(), debounce),
                pending.take_overflowed(),
            )
        };

        if overflowed {
            tracing::warn!(
                target: "notedthat::watch",
                "FS_WATCH_RESCAN: more changed at once than was worth tracking individually; \
                 re-examining the whole knowledge base"
            );
        }

        for signal in settled {
            // Deliberately not `try_send`. A busy consumer should slow this loop down, not
            // make it discard work: there is no client waiting on a filesystem change, so
            // a dropped one would simply never be noticed by anybody.
            if signals.send(signal).await.is_err() {
                return;
            }
        }
    }
}

/// Turn one kernel event into pending work.
fn handle_event(
    event: &Event,
    watched: &[Watched],
    directories: &mut HashSet<PathBuf>,
    pending: &mut pending::Pending,
    now: Instant,
) {
    // Checked before the kind filter: a rescan flag arrives as an otherwise meaningless
    // event kind, and it is the most important thing the kernel ever says.
    if event.need_rescan() {
        tracing::warn!(
            target: "notedthat::watch",
            "FS_WATCH_RESCAN: the kernel dropped filesystem events; re-examining every \
             knowledge base"
        );
        for entry in watched {
            pending.whole_kb(&entry.kb, now);
        }
        return;
    }

    if !is_change(event.kind) {
        return;
    }

    for path in &event.paths {
        let Some((kb, key)) = key_for(watched, path) else {
            continue;
        };

        if path.is_dir() {
            directories.insert(path.clone());
            pending.prefix(kb, format!("{key}/"), now);
            continue;
        }
        // Gone, and we knew it as a directory: the files it held raised no events of their
        // own, so the subtree has to be re-examined rather than the name itself.
        if directories.remove(path) {
            pending.prefix(kb, format!("{key}/"), now);
            continue;
        }

        // A name this adapter would never write and cannot read back is skipped:
        // reporting it would only produce work nothing can act on.
        if let Ok(key) = ObjectPath::try_from(key.as_str()) {
            pending.key(kb, key, now);
        }
    }
}

/// Turn a watcher error into pending work, loudly.
fn handle_error(
    error: &notify::Error,
    watched: &[Watched],
    pending: &mut pending::Pending,
    now: Instant,
) {
    if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
        // The kernel refused a watch on a directory that appeared after startup, and
        // `notify` stops trying after the first refusal. Everything created below that
        // point is now invisible, and only the operator can fix it.
        tracing::error!(
            target: "notedthat::watch",
            %error,
            "FS_WATCH_LOST: out of filesystem watches, so changes below newly created \
             directories will go unnoticed. Raise fs.inotify.max_user_watches, or set \
             NOTEDTHAT_FS_WATCH=false to stop watching deliberately."
        );
    } else {
        tracing::warn!(target: "notedthat::watch", %error, "FS_WATCH_LOST: watch error");
    }

    for entry in watched {
        pending.whole_kb(&entry.kb, now);
    }
}

/// Whether an event kind means bytes may have moved.
///
/// A reject-list on purpose — see the module documentation. `Access` is the one that
/// matters: without it, reading an object would count as changing it.
fn is_change(kind: EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

/// Which knowledge base and object key `path` names, if any.
///
/// Pure, so the rules that decide what `NotedThat` will look at can be tested without a
/// filesystem or a kernel subscription. Returns the key as a string rather than an
/// [`ObjectPath`] because a directory's key is a valid prefix but not a valid object.
fn key_for<'a>(watched: &'a [Watched], path: &Path) -> Option<(&'a KbSlug, String)> {
    let entry = watched
        .iter()
        .find(|entry| path.starts_with(&entry.dir) && path != entry.dir)?;

    let relative = path.strip_prefix(&entry.dir).ok()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        let name = name.to_str()?;
        // Our own in-flight writes, and crash leftovers of them, are never objects.
        if name.starts_with(TEMP_PREFIX) {
            return None;
        }
        if VCS_DIRS.contains(&name) {
            return None;
        }
    }

    let key = key_from_path(&entry.dir, path)?;
    // Private per D48, and how the manifest is excluded: it is an ordinary file inside the
    // knowledge base's directory, but nothing outside this adapter should react to it.
    if is_internal_path(&key) {
        return None;
    }
    Some((&entry.kb, key))
}

/// Record every directory at or below `root`, so a directory that later disappears can
/// still be recognised as one.
///
/// Best effort: an unreadable directory is skipped rather than failing the watch, matching
/// how listings treat one.
fn collect_directories(root: &Path, into: &mut HashSet<PathBuf>) {
    if !root.is_dir() {
        return;
    }
    into.insert(root.to_path_buf());
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect_directories(&path, into);
        }
    }
}

/// How many directories the watches have to cover, for the diagnostic.
fn directory_count(watched: &[Watched]) -> usize {
    let mut directories = HashSet::new();
    for entry in watched {
        collect_directories(&entry.dir, &mut directories);
    }
    directories.len()
}

fn watch_error(error: &notify::Error, directories: usize) -> Error {
    let advice = if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
        format!(
            " — this tree needs {directories} filesystem watches. Raise \
             fs.inotify.max_user_watches, or set NOTEDTHAT_FS_WATCH=false to serve without \
             watching."
        )
    } else {
        String::new()
    };
    Error::Config {
        message: format!("could not watch the storage root: {error}{advice}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched() -> Vec<Watched> {
        vec![
            Watched {
                kb: KbSlug::try_new("notes").expect("slug"),
                dir: PathBuf::from("/srv/nt/nt-default-notes"),
            },
            Watched {
                kb: KbSlug::try_new("other").expect("slug"),
                dir: PathBuf::from("/srv/nt/nt-default-other"),
            },
        ]
    }

    fn key_of(path: &str) -> Option<(String, String)> {
        key_for(&watched(), Path::new(path)).map(|(kb, key)| (kb.as_str().to_owned(), key))
    }

    #[test]
    fn an_object_maps_to_its_knowledge_base_and_key() {
        assert_eq!(
            key_of("/srv/nt/nt-default-notes/a/b.md"),
            Some(("notes".to_owned(), "a/b.md".to_owned()))
        );
        assert_eq!(
            key_of("/srv/nt/nt-default-other/c.md"),
            Some(("other".to_owned(), "c.md".to_owned()))
        );
    }

    #[test]
    fn a_path_under_no_watched_knowledge_base_is_ignored() {
        assert_eq!(
            key_of("/srv/nt/.notedthat-meta/nt-default-notes/a.md.ntmeta"),
            None
        );
        assert_eq!(key_of("/srv/nt/.notedthat.lock"), None);
        assert_eq!(key_of("/etc/passwd"), None);
    }

    #[test]
    fn a_watched_directory_itself_is_not_an_object() {
        assert_eq!(key_of("/srv/nt/nt-default-notes"), None);
    }

    /// Our own in-flight writes. Every commit renames one of these into place, so failing
    /// to skip them would report a key that never exists.
    #[test]
    fn a_temp_file_is_ignored_at_any_depth() {
        assert_eq!(key_of("/srv/nt/nt-default-notes/.notedthat-tmp-abc"), None);
        assert_eq!(
            key_of("/srv/nt/nt-default-notes/deep/.notedthat-tmp-abc"),
            None
        );
    }

    /// The documentation recommends version control, and one commit rewrites hundreds of
    /// files under `.git`.
    #[test]
    fn a_version_control_directory_is_ignored() {
        assert_eq!(key_of("/srv/nt/nt-default-notes/.git/index"), None);
        assert_eq!(
            key_of("/srv/nt/nt-default-notes/sub/.git/objects/ab/cdef"),
            None
        );
        assert_eq!(key_of("/srv/nt/nt-default-notes/.hg/store"), None);
    }

    #[test]
    fn the_manifest_and_everything_private_is_ignored() {
        assert_eq!(
            key_of("/srv/nt/nt-default-notes/.notedthat/manifest.json"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_ignored() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut path = PathBuf::from("/srv/nt/nt-default-notes");
        path.push(OsStr::from_bytes(b"bad\xff.md"));
        assert_eq!(key_for(&watched(), &path), None);
    }

    /// The single most important line in the filter. `notify`'s inotify mask always
    /// includes `IN_OPEN`, so without this, serving a read — or one `grep -r` over the
    /// tree — would re-examine the entire knowledge base.
    #[test]
    fn reading_an_object_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode};

        assert!(!is_change(EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_change(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
    }

    #[test]
    fn a_permission_change_is_not_a_change() {
        use notify::event::MetadataKind;

        assert!(!is_change(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Permissions
        ))));
    }

    #[test]
    fn writes_renames_and_removals_are_changes() {
        use notify::event::{CreateKind, DataChange, RemoveKind, RenameMode};

        assert!(is_change(EventKind::Create(CreateKind::File)));
        assert!(is_change(EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(is_change(EventKind::Modify(ModifyKind::Name(
            RenameMode::From
        ))));
        assert!(is_change(EventKind::Remove(RemoveKind::File)));
        // An unclassified event could be anything, including a write.
        assert!(is_change(EventKind::Any));
    }
}
