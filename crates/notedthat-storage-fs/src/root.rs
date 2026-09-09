//! Startup validation of the storage root, and the claim that keeps one process on it.
//!
//! Conditional writes are made atomic by an in-process lock (see [`crate::locks`]). That
//! guarantee holds for exactly one process, so a second server on the same root would
//! reintroduce precisely the silent lost writes that SPECIFICATIONS.md §8.1 warns about
//! for backends without a consensus mechanism. Rather than degrade quietly, refuse.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use notedthat_core::Error;

use crate::config::{FS_ALLOW_LOSSY_NAMES_ENV, FS_ROOT_ENV, FsConfig};
use crate::layout::{LOCK_FILE, META_DIR};

/// An exclusive claim on a storage root, held for the life of the process.
///
/// Dropping it releases the claim, so bind it to a named local rather than `_`.
#[derive(Debug)]
#[must_use = "dropping the RootLock releases the exclusive claim on the storage root"]
pub struct RootLock {
    _file: File,
    root: PathBuf,
}

impl RootLock {
    /// The canonicalized root this lock covers.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Prove the root is usable, then claim it for this process.
///
/// Validation is behavioural rather than introspective — it tries the operations instead
/// of inspecting metadata bits — matching how `StagingConfig::validate` checks the upload
/// directory. Probes run before the lock is taken, so a misconfigured root never gets a
/// lock file littered into it.
///
/// # Errors
///
/// Returns [`Error::Config`] when the root is missing, is not a directory, is not
/// writable, folds case or normalizes Unicode, or is already claimed by another process.
pub async fn open_root(config: &FsConfig) -> Result<RootLock, Error> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || open_root_blocking(&config))
        .await
        .map_err(|error| Error::Config {
            message: format!("storage root validation task failed: {error}"),
        })?
}

fn open_root_blocking(config: &FsConfig) -> Result<RootLock, Error> {
    let root = &config.root;

    let metadata = std::fs::metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            config_error(format!("{FS_ROOT_ENV} does not exist: {}", root.display()))
        } else {
            config_error(format!(
                "{FS_ROOT_ENV} is unreadable: {} ({error})",
                root.display()
            ))
        }
    })?;
    if !metadata.is_dir() {
        return Err(config_error(format!(
            "{FS_ROOT_ENV} is not a directory: {}",
            root.display()
        )));
    }

    let canonical = std::fs::canonicalize(root).map_err(|error| {
        config_error(format!(
            "{FS_ROOT_ENV} cannot be resolved: {} ({error})",
            root.display()
        ))
    })?;

    probe_writable(&canonical)?;
    if !config.allow_lossy_names {
        probe_name_fidelity(&canonical)?;
    }

    std::fs::create_dir_all(canonical.join(META_DIR)).map_err(|error| {
        config_error(format!(
            "{FS_ROOT_ENV} metadata directory could not be created: {error}"
        ))
    })?;

    claim(canonical)
}

/// Write and fsync a private temp file, the same probe the staging directory uses.
fn probe_writable(root: &Path) -> Result<(), Error> {
    let mut probe = tempfile::tempfile_in(root)
        .and_then(|mut file| {
            file.write_all(b"notedthat-storage-probe")?;
            Ok(file)
        })
        .map_err(|error| {
            config_error(format!(
                "{FS_ROOT_ENV} is not writable: {} ({error})",
                root.display()
            ))
        })?;
    probe
        .flush()
        .and_then(|()| probe.sync_all())
        .map_err(|error| {
            config_error(format!(
                "{FS_ROOT_ENV} did not accept a durable write: {} ({error})",
                root.display()
            ))
        })
}

/// Refuse a filesystem that would silently merge two distinct object keys.
///
/// `ObjectPath` compares byte-exactly, so on a case-folding filesystem (`APFS` and `NTFS` by
/// default) `Foo.md` and `foo.md` become one file and a PUT of the second destroys the
/// first — with a listing afterwards reporting a key nobody wrote. A normalizing
/// filesystem does the same to composed and decomposed Unicode. Both are silent data
/// loss, so they are a startup failure by default, with an explicit opt-out for an
/// operator who knows their key set avoids the hazard.
fn probe_name_fidelity(root: &Path) -> Result<(), Error> {
    let dir = tempfile::tempdir_in(root).map_err(|error| {
        config_error(format!(
            "{FS_ROOT_ENV} name-fidelity probe could not be created: {error}"
        ))
    })?;

    check_distinct(
        dir.path(),
        "notedthat-probe-A",
        "notedthat-probe-a",
        "folds letter case",
    )?;
    // "é" composed (U+00E9) against decomposed (U+0065 U+0301).
    check_distinct(
        dir.path(),
        "notedthat-probe-\u{e9}",
        "notedthat-probe-e\u{301}",
        "normalizes Unicode",
    )?;

    Ok(())
}

fn check_distinct(dir: &Path, written: &str, other: &str, what: &str) -> Result<(), Error> {
    let path = dir.join(written);
    File::create(&path).map_err(|error| {
        config_error(format!(
            "{FS_ROOT_ENV} name-fidelity probe could not be written: {error}"
        ))
    })?;

    if std::fs::symlink_metadata(dir.join(other)).is_ok() {
        return Err(config_error(format!(
            "the filesystem at {} {what}, so two distinct object keys would collide into one file and a write to either would destroy the other. \
             Use a filesystem that preserves names byte-for-byte, or set {FS_ALLOW_LOSSY_NAMES_ENV}=true to accept the risk.",
            dir.parent().unwrap_or(dir).display()
        )));
    }

    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn claim(root: PathBuf) -> Result<RootLock, Error> {
    let lock_path = root.join(LOCK_FILE);
    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| {
            config_error(format!(
                "cannot open the storage root lock at {}: {error}",
                lock_path.display()
            ))
        })?;

    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            let holder = std::fs::read_to_string(&lock_path).unwrap_or_default();
            let holder = holder
                .lines()
                .next()
                .unwrap_or("unknown")
                .trim()
                .to_string();
            return Err(config_error(format!(
                "the storage root {} is already in use by another notedthat-server process (PID {holder}). \
                 A filesystem storage root supports exactly one process: conditional writes are made atomic in-process, \
                 so a second one would silently lose writes. Stop the other process or give this one its own root.",
                root.display()
            )));
        }
        Err(std::fs::TryLockError::Error(error)) => {
            // Network filesystems are out of scope precisely because their advisory
            // locking is unreliable; failing here is the correct fail-fast.
            return Err(config_error(format!(
                "cannot lock the storage root {}: {error}. Network filesystems (NFS, SMB) are not supported.",
                root.display()
            )));
        }
    }

    // Advisory only — the lock, not this text, is what excludes a second process.
    let _ = file
        .set_len(0)
        .and_then(|()| writeln!(file, "{}", std::process::id()))
        .and_then(|()| file.sync_all());

    Ok(RootLock { _file: file, root })
}

fn config_error(message: String) -> Error {
    Error::Config { message }
}

#[cfg(test)]
mod tests {
    use super::open_root;
    use crate::config::FsConfig;

    #[tokio::test]
    async fn a_usable_root_is_claimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = open_root(&FsConfig::new(dir.path().to_path_buf()))
            .await
            .expect("root should be usable");
        assert!(lock.root().exists());
        assert!(dir.path().join(".notedthat-meta").is_dir());
    }

    #[tokio::test]
    async fn a_second_process_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FsConfig::new(dir.path().to_path_buf());
        let _held = open_root(&config).await.expect("first claim");

        // The lock is per-file-handle, so a second `open_root` models a second process.
        let error = open_root(&config)
            .await
            .expect_err("a second claim must be refused")
            .to_string();
        assert!(error.contains("already in use"), "{error}");
        assert!(error.contains("exactly one process"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_root_names_the_variable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FsConfig::new(dir.path().join("absent"));
        let error = open_root(&config).await.unwrap_err().to_string();
        assert!(
            error.contains("NOTEDTHAT_FS_ROOT does not exist"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_file_is_not_a_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-dir");
        std::fs::write(&path, b"x").expect("write");
        let error = open_root(&FsConfig::new(path))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("is not a directory"), "{error}");
    }
}
