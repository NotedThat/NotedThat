//! Durable, atomic replacement of a file, and the directory hygiene around it.
//!
//! Every write lands as: create a temp file *in the destination's own directory*, fill
//! it, set the browsable mode bits, fsync, rename over the destination, fsync the
//! directory. Renaming within one directory is atomic on POSIX and cannot fail with
//! `EXDEV`, so a reader either sees the whole old object or the whole new one.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use notedthat_core::StorageError;
use tempfile::NamedTempFile;

use crate::errors;
use crate::layout::TEMP_PREFIX;

/// Create `dir` and its parents with the configured mode.
pub(crate) fn create_dir_all(dir: &Path, mode: u32) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    builder.create(dir)
}

/// Start a temp file in `dir` that can be renamed over a sibling.
pub(crate) fn temp_in(dir: &Path) -> std::io::Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)
}

/// Give a staged file the mode a browsable tree needs.
///
/// `tempfile` creates at `0600`. Committing that would make every note unreadable to
/// anyone but the server user — no editing the store by hand, and no backup job that
/// runs as anyone else, which is most of the reason to choose this backend.
pub(crate) fn apply_mode(file: &File, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
        Ok(())
    }
}

/// Flush and fsync a staged file, then rename it over `destination` and fsync the parent.
///
/// Returns the staged file's metadata, read immediately before the rename. Callers stamp
/// their `ETag` record against it rather than against a stat of `destination` afterwards:
/// `rename` preserves size, mtime and inode, so the two describe the same bytes — but a
/// stat of the destination can pick up content another writer put there in between, which
/// would record our `ETag` against their file and then read back as fresh.
pub(crate) fn finish(
    mut staged: NamedTempFile,
    destination: &Path,
    mode: u32,
) -> std::io::Result<std::fs::Metadata> {
    staged.flush()?;
    apply_mode(staged.as_file(), mode)?;
    staged.as_file().sync_all()?;
    // After the last write and after `apply_mode`, which touches ctime only.
    let metadata = staged.as_file().metadata()?;
    staged.persist(destination).map_err(|error| error.error)?;
    sync_dir(destination.parent());
    Ok(metadata)
}

/// Replace `path` with `bytes`, durably and atomically.
pub(crate) fn replace_file(
    dir: &Path,
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<(), StorageError> {
    let mut staged = temp_in(dir).map_err(|error| errors::backend(&error))?;
    staged
        .write_all(bytes)
        .map_err(|error| errors::backend(&error))?;
    finish(staged, path, mode)
        .map(|_| ())
        .map_err(|error| errors::backend(&error))
}

/// fsync a directory so a rename inside it survives a crash.
///
/// Best effort: Windows cannot open a directory as a file, and some filesystems refuse.
/// The rename is still atomic either way; only its durability is at stake.
pub(crate) fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(dir) = dir
        && let Ok(handle) = File::open(dir)
    {
        let _ = handle.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Remove directories left empty by a delete, walking up to but not including `stop`.
///
/// Not cosmetic. A leftover `a/b/` keeps that name occupied as a directory, so after
/// deleting `a/b/c` a later `PUT a/b` would fail on this backend while succeeding on S3.
/// Pruning keeps the two backends' key spaces the same shape — and keeps a file manager
/// from showing a skeleton of empty folders, which matters for a tree meant to be read
/// by people.
pub(crate) fn prune_empty_dirs(from: &Path, stop: &Path) {
    let mut current = from;
    while current != stop && current.starts_with(stop) {
        // Fails with `DirectoryNotEmpty` as soon as a level still holds something, and
        // with `NotFound` if a concurrent prune got here first. Both end the walk.
        if std::fs::remove_dir(current).is_err() {
            return;
        }
        let Some(parent) = current.parent() else {
            return;
        };
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::{create_dir_all, prune_empty_dirs, replace_file};

    #[test]
    fn a_replaced_file_carries_the_configured_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.md");
        replace_file(dir.path(), &path, b"hello", 0o644).expect("replace");
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o644,
                "a 0600 temp file must not be committed"
            );
        }
    }

    #[test]
    fn replacing_an_existing_file_keeps_readers_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.md");
        replace_file(dir.path(), &path, b"first", 0o644).expect("first");
        replace_file(dir.path(), &path, b"second", 0o644).expect("second");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
    }

    #[test]
    fn pruning_stops_at_the_bucket_and_at_the_first_non_empty_level() {
        let root = tempfile::tempdir().expect("tempdir");
        let bucket = root.path().join("nt-t-kb");
        let deep = bucket.join("a/b/c");
        create_dir_all(&deep, 0o755).expect("dirs");
        std::fs::write(bucket.join("a/keep.md"), b"x").expect("write");

        prune_empty_dirs(&deep, &bucket);

        assert!(!bucket.join("a/b").exists(), "empty levels are removed");
        assert!(bucket.join("a").is_dir(), "a level with content survives");
        assert!(bucket.is_dir(), "the bucket directory is never removed");
    }

    #[test]
    fn pruning_an_already_removed_path_is_harmless() {
        let root = tempfile::tempdir().expect("tempdir");
        let bucket = root.path().join("nt-t-kb");
        create_dir_all(&bucket, 0o755).expect("dirs");
        prune_empty_dirs(&bucket.join("gone/deeper"), &bucket);
        assert!(bucket.is_dir());
    }
}
