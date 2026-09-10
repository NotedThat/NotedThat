//! Local-filesystem storage adapter for `NotedThat`.
//!
//! Implements [`notedthat_core::Storage`] against a directory tree, as an alternative to
//! an S3-compatible object store. An object's key *is* its path, so the store can be
//! opened in an editor, grepped, `rsync`ed, or put under version control:
//!
//! ```text
//! /srv/notedthat/nt-default-notes/notes/hello.md
//! ```
//!
//! # What it is for
//!
//! A single-node deployment that would otherwise run `SeaweedFS` or `MinIO` purely to keep
//! Markdown files on a disk the process can already see.
//!
//! # What it guarantees
//!
//! Conditional writes (`If-Match`, `If-None-Match`) are atomic — the property
//! SPECIFICATIONS.md §8.1 records as the hard one, which Garage and pre-4.09 `SeaweedFS` do
//! not provide and which D46's PATCH splice depends on. That guarantee is enforced by an
//! in-process lock, so it holds for **one process on one root**. [`open_root`] refuses to
//! start a second, rather than degrade into silent lost writes. Network filesystems are
//! out of scope for the same reason.
//!
//! # Editing the tree directly
//!
//! Supported, and the point of the backend. An object edited behind the server's back is
//! detected on the next request and its `ETag` recomputed from content, and the search
//! index keeps up too: [`watch`] notices the change and [`reconcile`] works out what it
//! means, so an edit made in an editor, by `git`, or by a script is searchable shortly
//! afterwards without being written through `NotedThat`.
//!
//! Every knowledge base is also compared against the index once at startup, since changes
//! made while the server was not running raise no events at all. That comparison is cheap
//! on a store that has not moved: the freshness stamp means an unchanged object costs a
//! stat and a small sidecar read, with its content never opened.
//!
//! Two limits are worth knowing. A file being written continuously — an in-place `rsync`
//! of a large one — may be indexed from partial content and corrected on a later pass.
//! And watching is supported on Linux and macOS; other platforms compile but are untested,
//! so set `NOTEDTHAT_FS_WATCH=false` there.

#![deny(missing_docs)]

mod commit;
pub mod config;
mod errors;
mod layout;
mod listing;
mod locks;
mod meta;
pub mod reconcile;
mod root;
mod storage;
pub mod watch;

pub use config::{
    FS_ALLOW_LOSSY_NAMES_ENV, FS_DIR_MODE_ENV, FS_ENV_VARS, FS_FILE_MODE_ENV, FS_METADATA_ENV,
    FS_ROOT_ENV, FS_WATCH_DEBOUNCE_MS_ENV, FS_WATCH_ENV, FsConfig, FsSettings, MetadataMode,
};
pub use reconcile::{FsChange, IndexedEtag, ReconcileReport, reconcile};
pub use root::{RootLock, open_root};
pub use storage::FsStorage;
pub use watch::{FsSignal, FsWatchConfig, FsWatcher, watch_kbs};
