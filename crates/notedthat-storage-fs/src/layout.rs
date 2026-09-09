//! Mapping object keys onto filesystem paths, and the guards that mapping needs.
//!
//! # Why the metadata tree sits outside the buckets
//!
//! ```text
//! <root>/
//!   .notedthat.lock              process lock; never inside a bucket
//!   .notedthat-meta/             metadata shadow tree
//!     nt-default-notes/
//!       notes/hello.md.ntmeta
//!   nt-default-notes/            = derive_bucket_name(tenant, kb)
//!     notes/hello.md             object key "notes/hello.md" — this IS the file
//!     .notedthat/manifest.json   object key ".notedthat/manifest.json", as on S3
//! ```
//!
//! Object keys map only into `<root>/<bucket>/…`, and every bucket name begins with
//! [`BUCKET_NAME_PREFIX`] (`nt-`), which `.notedthat-meta` and `.notedthat.lock` do not.
//! So no expressible [`ObjectPath`] can name a metadata file or the lock — and
//! `list_objects` needs no exclusion rules at all: every regular file under a bucket is
//! an object, including `.notedthat/manifest.json`, exactly as on S3.
//!
//! Putting sidecars *inside* the bucket would have made both of those false.

use std::path::{Component, Path, PathBuf};

use notedthat_core::{KbSlug, ObjectPath, StorageError, TenantSlug};

/// Directory under the root holding the metadata shadow tree.
pub(crate) const META_DIR: &str = ".notedthat-meta";

/// Lock file claiming the root for one process.
pub(crate) const LOCK_FILE: &str = ".notedthat.lock";

/// Suffix appended to an object key to name its sidecar.
pub(crate) const SIDECAR_SUFFIX: &str = ".ntmeta";

/// Prefix of in-flight temporary files. Reserved: a key whose final segment starts with
/// this is refused on write and skipped by listings, so a crash leftover is never served
/// as an object.
pub(crate) const TEMP_PREFIX: &str = ".notedthat-tmp-";

/// Longest path segment we will create.
///
/// 255 bytes is the limit on ext4, xfs, btrfs and `APFS`. The sidecar suffix has to fit
/// alongside the key's own final segment, and the limit is applied uniformly regardless
/// of metadata mode so that changing modes can never orphan an existing store.
pub(crate) const MAX_SEGMENT_BYTES: usize = 255 - SIDECAR_SUFFIX.len();

/// Resolved directories for one storage root.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    root: PathBuf,
    meta_root: PathBuf,
    file_mode: u32,
    dir_mode: u32,
}

impl Layout {
    /// Build a layout over an already-canonicalized root.
    pub(crate) fn new(root: PathBuf, file_mode: u32, dir_mode: u32) -> Self {
        let meta_root = root.join(META_DIR);
        Self {
            root,
            meta_root,
            file_mode,
            dir_mode,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Mode bits for files this adapter creates.
    ///
    /// `tempfile` creates at `0600`; committing that into a tree meant to be opened in
    /// an editor or read by a backup job would defeat the point of the layout.
    pub(crate) fn file_mode(&self) -> u32 {
        self.file_mode
    }

    pub(crate) fn dir_mode(&self) -> u32 {
        self.dir_mode
    }

    /// Directory holding one KB's objects.
    pub(crate) fn bucket_dir(&self, bucket: &str) -> PathBuf {
        self.root.join(bucket)
    }

    /// Directory holding one KB's sidecars.
    pub(crate) fn meta_bucket_dir(&self, bucket: &str) -> PathBuf {
        self.meta_root.join(bucket)
    }

    /// Absolute path of the file backing `key`.
    pub(crate) fn object_path(&self, bucket: &str, key: &str) -> Result<PathBuf, StorageError> {
        push_key(self.bucket_dir(bucket), key)
    }

    /// Absolute path of the sidecar recording `key`'s metadata.
    pub(crate) fn sidecar_path(&self, bucket: &str, key: &str) -> Result<PathBuf, StorageError> {
        let mut path = push_key(self.meta_bucket_dir(bucket), key)?;
        let mut name = path
            .file_name()
            .expect("a pushed key always has a final segment")
            .to_os_string();
        name.push(SIDECAR_SUFFIX);
        path.set_file_name(name);
        Ok(path)
    }
}

/// Derived directory name for a KB — the same string S3 uses as the bucket name.
///
/// Reusing it means a KB's on-disk directory and its S3 bucket carry one name, so an
/// operator moving between backends is looking at the same identifier in both places.
pub(crate) fn bucket_name(tenant: &TenantSlug, kb: &KbSlug) -> String {
    notedthat_core::derive_bucket_name(tenant, kb)
}

/// Append an object key to `base`, one segment at a time.
///
/// Deliberately not `base.join(key)`: on Windows a segment such as `C:` is a path prefix
/// and `PathBuf::push` would *replace* everything accumulated so far, which is a
/// traversal escape. Pushing segment by segment and rejecting anything that is not a
/// plain name closes that.
fn push_key(base: PathBuf, key: &str) -> Result<PathBuf, StorageError> {
    let mut path = base;
    for segment in key.split('/') {
        validate_segment(segment, key)?;
        path.push(segment);
    }
    Ok(path)
}

fn validate_segment(segment: &str, key: &str) -> Result<(), StorageError> {
    if segment.len() > MAX_SEGMENT_BYTES {
        return Err(unsupported_key(format!(
            "path segment '{segment}' of key '{key}' is {} bytes, over the {MAX_SEGMENT_BYTES}-byte filesystem limit",
            segment.len()
        )));
    }

    if segment.starts_with(TEMP_PREFIX) {
        return Err(unsupported_key(format!(
            "key '{key}' uses the reserved '{TEMP_PREFIX}' prefix"
        )));
    }

    // ObjectPath already rejects empty segments, `.`, `..`, backslashes and NUL. This
    // catches anything else that is not a plain filename — a drive prefix, chiefly.
    let mut components = Path::new(segment).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name == segment => Ok(()),
        _ => Err(unsupported_key(format!(
            "path segment '{segment}' of key '{key}' is not a plain filename"
        ))),
    }
}

/// A key that S3 would accept but this filesystem cannot represent.
pub(crate) fn unsupported_key(message: String) -> StorageError {
    StorageError::Other {
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        )),
    }
}

/// Recover the object key from a path below a bucket directory.
///
/// Returns `None` for a name that is not valid UTF-8 — such a file cannot be an
/// [`ObjectPath`] and is therefore invisible to `NotedThat`.
pub(crate) fn key_from_path(bucket_dir: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(bucket_dir).ok()?;
    let mut segments = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => segments.push(name.to_str()?),
            _ => return None,
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

/// Convert an [`ObjectPath`] to the key string used throughout the adapter.
pub(crate) fn key_of(path: &ObjectPath) -> &str {
    path.as_str()
}

#[cfg(test)]
mod tests {
    use super::{LOCK_FILE, Layout, MAX_SEGMENT_BYTES, META_DIR, key_from_path};
    use std::path::{Path, PathBuf};

    fn layout() -> Layout {
        Layout::new(PathBuf::from("/srv/nt"), 0o644, 0o755)
    }

    #[test]
    fn object_and_sidecar_live_in_separate_trees() {
        let layout = layout();
        assert_eq!(
            layout
                .object_path("nt-default-notes", "notes/hello.md")
                .unwrap(),
            Path::new("/srv/nt/nt-default-notes/notes/hello.md")
        );
        assert_eq!(
            layout
                .sidecar_path("nt-default-notes", "notes/hello.md")
                .unwrap(),
            Path::new("/srv/nt/.notedthat-meta/nt-default-notes/notes/hello.md.ntmeta")
        );
    }

    #[test]
    fn the_manifest_is_an_ordinary_object_on_disk() {
        assert_eq!(
            layout()
                .object_path("nt-default-notes", ".notedthat/manifest.json")
                .unwrap(),
            Path::new("/srv/nt/nt-default-notes/.notedthat/manifest.json")
        );
    }

    /// The layout invariant the whole design rests on: object keys resolve only under
    /// `<root>/<bucket>/`, and neither the metadata tree nor the lock lives there,
    /// because both sit at the root and no bucket name can begin with a dot.
    #[test]
    fn no_key_can_name_the_metadata_tree_or_the_lock() {
        use notedthat_core::BUCKET_NAME_PREFIX;
        assert!(!META_DIR.starts_with(BUCKET_NAME_PREFIX));
        assert!(!LOCK_FILE.starts_with(BUCKET_NAME_PREFIX));

        let layout = layout();
        let bucket = layout.bucket_dir("nt-default-notes");
        for key in [".notedthat-meta/x", ".notedthat.lock", "a/b.md"] {
            let resolved = layout.object_path("nt-default-notes", key).expect("mapped");
            assert!(resolved.starts_with(&bucket), "{key} escaped the bucket");
        }
    }

    #[test]
    fn an_over_long_segment_is_refused() {
        let long = "a".repeat(MAX_SEGMENT_BYTES + 1);
        assert!(layout().object_path("nt-default-notes", &long).is_err());
        let ok = "a".repeat(MAX_SEGMENT_BYTES);
        assert!(layout().object_path("nt-default-notes", &ok).is_ok());
    }

    #[test]
    fn the_temp_prefix_is_reserved() {
        assert!(
            layout()
                .object_path("nt-default-notes", ".notedthat-tmp-x")
                .is_err()
        );
    }

    #[test]
    fn keys_round_trip_through_paths() {
        let bucket = Path::new("/srv/nt/nt-default-notes");
        assert_eq!(
            key_from_path(bucket, Path::new("/srv/nt/nt-default-notes/a/b.md")).as_deref(),
            Some("a/b.md")
        );
        assert_eq!(key_from_path(bucket, bucket), None);
    }

    #[test]
    fn unicode_and_spaces_survive_the_mapping() {
        assert_eq!(
            layout()
                .object_path("nt-default-notes", "dossier/文 note.md")
                .unwrap(),
            Path::new("/srv/nt/nt-default-notes/dossier/文 note.md")
        );
    }
}
