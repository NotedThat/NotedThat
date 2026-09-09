//! Mapping `io::Error` onto [`StorageError`].
//!
//! The dividing line, so it can be applied to cases not enumerated here:
//!
//! - [`backend`] — the store as a whole is unusable right now and retrying later might
//!   work: permissions, a full disk, a read-only mount. `S3Storage` additionally maps
//!   *every* failure of `ensure_bucket`, the manifest calls and `list_objects` this way,
//!   so those paths do the same here.
//! - [`other`] — this request cannot be expressed on a filesystem and retrying is
//!   pointless: a key colliding with a directory, a name too long for the platform.
//!
//! Note what is deliberately *not* `BackendUnavailable`: a failure on the object data
//! path. `S3Storage` maps a 500 or a timeout on GET/PUT/DELETE to `Other`, and matching
//! that keeps the two backends' error contracts identical.

use std::io::ErrorKind;

use notedthat_core::StorageError;

/// The store is unusable right now.
pub(crate) fn backend(error: &std::io::Error) -> StorageError {
    StorageError::BackendUnavailable {
        message: error.to_string(),
    }
}

/// The store is unusable right now, with context naming what was attempted.
pub(crate) fn backend_at(context: &str, error: &std::io::Error) -> StorageError {
    StorageError::BackendUnavailable {
        message: format!("{context}: {error}"),
    }
}

/// This request cannot be expressed on a filesystem.
pub(crate) fn other(error: std::io::Error) -> StorageError {
    StorageError::Other {
        source: Box::new(error),
    }
}

/// Map an error from the object data path, given the key it concerns.
///
/// `NotFound` is the caller's business to interpret: `delete_object` swallows it to stay
/// idempotent, every read path reports it.
pub(crate) fn object(key: &str, error: std::io::Error) -> StorageError {
    match error.kind() {
        ErrorKind::NotFound => StorageError::NotFound {
            key: key.to_string(),
        },
        // A key whose parent is an existing object, or that is itself a directory. S3
        // permits `a/b` and `a/b/c` to coexist; a filesystem cannot represent both.
        ErrorKind::NotADirectory | ErrorKind::IsADirectory => {
            crate::layout::unsupported_key(format!(
                "key '{key}' collides with a directory in the storage tree \
                 (S3 allows an object and a prefix to share a name; a filesystem cannot): {error}"
            ))
        }
        ErrorKind::PermissionDenied
        | ErrorKind::ReadOnlyFilesystem
        | ErrorKind::StorageFull
        | ErrorKind::QuotaExceeded => backend(&error),
        _ => other(error),
    }
}

#[cfg(test)]
mod tests {
    use super::object;
    use notedthat_core::StorageError;
    use std::io::{Error, ErrorKind};

    #[test]
    fn a_missing_file_is_not_found_with_its_key() {
        let mapped = object("a.md", Error::from(ErrorKind::NotFound));
        assert!(matches!(mapped, StorageError::NotFound { key } if key == "a.md"));
    }

    #[test]
    fn a_directory_collision_explains_the_s3_difference() {
        let mapped = object("a/b", Error::from(ErrorKind::NotADirectory));
        let StorageError::Other { source } = mapped else {
            panic!("expected Other");
        };
        assert!(source.to_string().contains("collides with a directory"));
    }

    #[test]
    fn an_unwritable_store_is_backend_unavailable() {
        assert!(matches!(
            object("a.md", Error::from(ErrorKind::PermissionDenied)),
            StorageError::BackendUnavailable { .. }
        ));
        assert!(matches!(
            object("a.md", Error::from(ErrorKind::StorageFull)),
            StorageError::BackendUnavailable { .. }
        ));
    }

    #[test]
    fn an_unclassified_data_path_failure_matches_s3_and_stays_other() {
        assert!(matches!(
            object("a.md", Error::from(ErrorKind::Interrupted)),
            StorageError::Other { .. }
        ));
    }
}
