//! Per-object metadata: what the file itself cannot tell us, plus a validity stamp.
//!
//! # Why the stamp exists
//!
//! The tree is meant to be browsable, so people will edit files in it directly. An
//! `ETag` recorded when the server last wrote an object is therefore not trustworthy on
//! its own — the bytes may have changed since.
//!
//! Every record carries the file state it was computed over (size, mtime, inode). On
//! read, that is compared against the file: matching means the recorded `ETag` is still
//! good, differing means the object changed underneath us and the `ETag` is recomputed
//! from content. So an out-of-band edit produces a correct new `ETag` on the next
//! request rather than a stale one, and no repair pass is needed.
//!
//! The same check is what makes the two-step sidecar commit safe. A sidecar cannot be
//! written in the same syscall as its object, so a crash between the two renames leaves
//! new content beside an old record — which the stamp detects as stale and repairs. That
//! is why `mtime_nanos` and `ino` are part of it: without them, new content that happens
//! to land on the same size and whole-second mtime would read as fresh.

use std::fs::Metadata;
use std::path::Path;
use std::time::SystemTime;

use notedthat_core::{StorageError, unix_seconds_i64};
use serde::{Deserialize, Serialize};

use crate::config::MetadataMode;
use crate::layout::Layout;

/// What we record about an object, and the file state it was recorded against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ObjectAttrs {
    /// Schema version.
    pub(crate) v: u8,
    /// Quoted SHA-256 of the content.
    pub(crate) etag: String,
    /// Content type as supplied on write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<String>,
    /// Size the `ETag` was computed over.
    pub(crate) size: u64,
    /// Whole seconds of the mtime the `ETag` was computed over.
    pub(crate) mtime_secs: i64,
    /// Sub-second remainder of that mtime.
    pub(crate) mtime_nanos: u32,
    /// Inode, where the platform has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ino: Option<u64>,
}

/// Schema version written by this build.
const SCHEMA_VERSION: u8 = 1;

impl ObjectAttrs {
    pub(crate) fn new(etag: String, content_type: Option<String>, metadata: &Metadata) -> Self {
        let (mtime_secs, mtime_nanos) = split_mtime(metadata);
        Self {
            v: SCHEMA_VERSION,
            etag,
            content_type,
            size: metadata.len(),
            mtime_secs,
            mtime_nanos,
            ino: inode(metadata),
        }
    }

    /// Whether this record still describes the file on disk.
    fn is_fresh_for(&self, metadata: &Metadata) -> bool {
        if self.v != SCHEMA_VERSION || self.size != metadata.len() {
            return false;
        }
        let (secs, nanos) = split_mtime(metadata);
        if self.mtime_secs != secs || self.mtime_nanos != nanos {
            return false;
        }
        match (self.ino, inode(metadata)) {
            (Some(recorded), Some(actual)) => recorded == actual,
            _ => true,
        }
    }
}

fn split_mtime(metadata: &Metadata) -> (i64, u32) {
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let nanos = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos());
    (unix_seconds_i64(modified), nanos)
}

// `Option` because the non-unix build has no inode to report.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn inode(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn inode(_metadata: &Metadata) -> Option<u64> {
    None
}

/// Where metadata is kept. One variant today; the mode is configuration so that adding
/// another does not change the configuration surface.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetaStore {
    mode: MetadataMode,
}

impl MetaStore {
    pub(crate) fn new(mode: MetadataMode) -> Self {
        Self { mode }
    }

    /// Resolve an object's attributes, repairing them when the file has moved on.
    ///
    /// `hash` recomputes the `ETag` from the file's current content; it is only called
    /// when the record is missing or stale.
    pub(crate) fn resolve(
        self,
        layout: &Layout,
        bucket: &str,
        key: &str,
        metadata: &Metadata,
        hash: impl FnOnce() -> Result<String, StorageError>,
    ) -> Result<ObjectAttrs, StorageError> {
        let recorded = self.load(layout, bucket, key);

        if let Some(attrs) = &recorded
            && attrs.is_fresh_for(metadata)
        {
            return Ok(attrs.clone());
        }

        let etag = hash()?;
        // The bytes changed, not the declared type — keep whatever was recorded.
        let content_type = recorded
            .as_ref()
            .and_then(|attrs| attrs.content_type.clone())
            .or_else(|| guess_content_type(key));
        let repaired = ObjectAttrs::new(etag, content_type, metadata);

        // Best effort: a read-only mount must still serve reads.
        if let Err(error) = self.write(layout, bucket, key, &repaired) {
            tracing::debug!(key, %error, "could not repair object metadata");
        }
        Ok(repaired)
    }

    fn load(self, layout: &Layout, bucket: &str, key: &str) -> Option<ObjectAttrs> {
        match self.mode {
            MetadataMode::Sidecar => {
                let path = layout.sidecar_path(bucket, key).ok()?;
                let bytes = std::fs::read(&path).ok()?;
                match serde_json::from_slice(&bytes) {
                    Ok(attrs) => Some(attrs),
                    Err(error) => {
                        tracing::warn!(key, %error, "ignoring unreadable object metadata");
                        None
                    }
                }
            }
        }
    }

    /// Write a record, replacing any existing one atomically.
    pub(crate) fn write(
        self,
        layout: &Layout,
        bucket: &str,
        key: &str,
        attrs: &ObjectAttrs,
    ) -> Result<(), StorageError> {
        match self.mode {
            MetadataMode::Sidecar => {
                let path = layout.sidecar_path(bucket, key)?;
                let parent = path.parent().ok_or_else(|| {
                    crate::layout::unsupported_key(format!(
                        "metadata path for '{key}' has no parent"
                    ))
                })?;
                std::fs::create_dir_all(parent).map_err(|error| crate::errors::backend(&error))?;
                let bytes = serde_json::to_vec(attrs).map_err(|error| StorageError::Other {
                    source: Box::new(error),
                })?;
                crate::commit::replace_file(parent, &path, &bytes, layout.file_mode())
            }
        }
    }

    /// Remove a record. Absent is success.
    pub(crate) fn remove(
        self,
        layout: &Layout,
        bucket: &str,
        key: &str,
    ) -> Result<(), StorageError> {
        match self.mode {
            MetadataMode::Sidecar => {
                let path = layout.sidecar_path(bucket, key)?;
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(crate::errors::backend(&error)),
                }
                if let Some(parent) = path.parent() {
                    crate::commit::prune_empty_dirs(parent, &layout.meta_bucket_dir(bucket));
                }
                Ok(())
            }
        }
    }
}

/// Content type for an object whose recorded type was lost.
///
/// Deliberately minimal: this only fires for objects written outside the server, where
/// no type was ever declared.
fn guess_content_type(key: &str) -> Option<String> {
    let extension = Path::new(key).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "md" | "markdown" => Some("text/markdown".to_string()),
        "txt" => Some("text/plain".to_string()),
        "json" => Some("application/json".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{MetaStore, ObjectAttrs, guess_content_type};
    use crate::config::MetadataMode;
    use crate::layout::Layout;
    use notedthat_core::compute_etag;

    fn setup() -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().to_path_buf(), 0o644, 0o755);
        std::fs::create_dir_all(layout.bucket_dir("nt-t-kb")).expect("bucket");
        (dir, layout)
    }

    fn write_object(layout: &Layout, key: &str, body: &[u8]) -> std::fs::Metadata {
        let path = layout.object_path("nt-t-kb", key).expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(&path, body).expect("write");
        std::fs::symlink_metadata(&path).expect("stat")
    }

    #[test]
    fn a_fresh_record_is_used_without_rehashing() {
        let (_dir, layout) = setup();
        let store = MetaStore::new(MetadataMode::Sidecar);
        let metadata = write_object(&layout, "a.md", b"hello");
        let attrs = ObjectAttrs::new(
            compute_etag(b"hello"),
            Some("text/markdown".into()),
            &metadata,
        );
        store
            .write(&layout, "nt-t-kb", "a.md", &attrs)
            .expect("write meta");

        let resolved = store
            .resolve(&layout, "nt-t-kb", "a.md", &metadata, || {
                panic!("a fresh record must not be rehashed")
            })
            .expect("resolve");
        assert_eq!(resolved.etag, compute_etag(b"hello"));
        assert_eq!(resolved.content_type.as_deref(), Some("text/markdown"));
    }

    #[test]
    fn an_edit_behind_our_back_yields_a_new_etag_and_keeps_the_content_type() {
        let (_dir, layout) = setup();
        let store = MetaStore::new(MetadataMode::Sidecar);
        let metadata = write_object(&layout, "a.md", b"hello");
        let attrs = ObjectAttrs::new(
            compute_etag(b"hello"),
            Some("text/markdown".into()),
            &metadata,
        );
        store
            .write(&layout, "nt-t-kb", "a.md", &attrs)
            .expect("write meta");

        // Someone opens the file in an editor and saves.
        let edited = write_object(&layout, "a.md", b"hello, again");
        let resolved = store
            .resolve(&layout, "nt-t-kb", "a.md", &edited, || {
                Ok(compute_etag(b"hello, again"))
            })
            .expect("resolve");
        assert_eq!(resolved.etag, compute_etag(b"hello, again"));
        assert_eq!(resolved.content_type.as_deref(), Some("text/markdown"));
    }

    #[test]
    fn a_missing_record_is_rebuilt_and_the_type_guessed() {
        let (_dir, layout) = setup();
        let store = MetaStore::new(MetadataMode::Sidecar);
        let metadata = write_object(&layout, "notes/new.md", b"dropped in by hand");

        let resolved = store
            .resolve(&layout, "nt-t-kb", "notes/new.md", &metadata, || {
                Ok(compute_etag(b"dropped in by hand"))
            })
            .expect("resolve");
        assert_eq!(resolved.etag, compute_etag(b"dropped in by hand"));
        assert_eq!(resolved.content_type.as_deref(), Some("text/markdown"));

        // And the repair was persisted, so the next read costs no hashing.
        let again = store
            .resolve(&layout, "nt-t-kb", "notes/new.md", &metadata, || {
                panic!("the repaired record should be fresh")
            })
            .expect("resolve");
        assert_eq!(again.etag, resolved.etag);
    }

    #[test]
    fn a_corrupt_record_is_treated_as_missing() {
        let (_dir, layout) = setup();
        let store = MetaStore::new(MetadataMode::Sidecar);
        let metadata = write_object(&layout, "a.md", b"hello");
        let sidecar = layout.sidecar_path("nt-t-kb", "a.md").expect("path");
        std::fs::create_dir_all(sidecar.parent().expect("parent")).expect("dirs");
        std::fs::write(&sidecar, b"{ not json").expect("write");

        let resolved = store
            .resolve(&layout, "nt-t-kb", "a.md", &metadata, || {
                Ok(compute_etag(b"hello"))
            })
            .expect("resolve");
        assert_eq!(resolved.etag, compute_etag(b"hello"));
    }

    #[test]
    fn content_type_is_only_guessed_for_types_we_serve() {
        assert_eq!(guess_content_type("a.md").as_deref(), Some("text/markdown"));
        assert_eq!(guess_content_type("a.bin"), None);
        assert_eq!(guess_content_type("noext"), None);
    }
}
