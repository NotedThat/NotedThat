//! Domain error types: `Error` and `StorageError`.

use thiserror::Error;

/// Domain error mapped to §6.12 HTTP status codes.
/// This is the general error for the API layer — storage-specific errors use [`StorageError`].
#[derive(Debug, Error)]
pub enum Error {
    /// Input from the client was invalid (e.g., malformed slug, invalid path).
    #[error("invalid input: {message}")]
    InvalidInput {
        /// Human-readable description of what was invalid.
        message: String,
    },

    /// The requested resource was not found.
    #[error("not found: {resource}")]
    NotFound {
        /// Identifies the missing resource (e.g., `"kb:my-notes"`).
        resource: String,
    },

    /// The request payload exceeded the allowed size limit.
    #[error("payload too large: {size} bytes (limit {limit})")]
    PayloadTooLarge {
        /// The actual payload size in bytes.
        size: u64,
        /// The maximum allowed size in bytes.
        limit: u64,
    },

    /// The derived S3 bucket name exceeds 63 characters.
    #[error("bucket name too long: {name} ({len} chars, max 63)")]
    BucketNameTooLong {
        /// The full bucket name that was too long.
        name: String,
        /// The length in bytes of the too-long name.
        len: usize,
    },

    /// A required configuration value was missing or invalid.
    #[error("configuration error: {message}")]
    Config {
        /// Human-readable description of the configuration problem.
        message: String,
    },

    /// A storage-layer error (see [`StorageError`]).
    #[error(transparent)]
    Storage(StorageError),

    /// Malformed `Range: bytes=` header — unparseable syntax. Maps to HTTP 400.
    #[error("malformed Range header: {0}")]
    MalformedRange(String),

    /// Backend returned 304 Not Modified (conditional request). Maps to HTTP 304.
    #[error("not modified")]
    NotModified,

    /// Backend returned 412 Precondition Failed (conditional request). Maps to HTTP 412.
    #[error("precondition failed")]
    PreconditionFailed,

    /// Backend returned 416 Range Not Satisfiable. `complete_length` is the total object size.
    /// Maps to HTTP 416 with `Content-Range: bytes */complete_length`.
    #[error("range not satisfiable (object size: {complete_length})")]
    RangeNotSatisfiable {
        /// The total size of the object in bytes.
        complete_length: u64,
    },
}

/// Storage-layer error — distinct from [`enum@Error`] so that different backends
/// (S3, in-memory mock, future prefix-per-KB) share a stable failure surface.
#[derive(Debug)]
pub enum StorageError {
    /// The requested object was not found in storage.
    NotFound {
        /// The key of the missing object.
        key: String,
    },

    /// The storage bucket for the KB was not found.
    BucketNotFound {
        /// The bucket name that was not found.
        bucket: String,
    },

    /// The storage backend is temporarily unavailable.
    BackendUnavailable {
        /// The underlying error message from the backend.
        message: String,
    },

    /// An unexpected storage error. The inner error provides details.
    Other {
        /// The root cause.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// S3 backend returned 304 Not Modified.
    NotModified,

    /// S3 backend returned 412 Precondition Failed.
    PreconditionFailed,

    /// S3 backend returned 416 Range Not Satisfiable. `complete_length` is the total object size
    /// extracted from the `Content-Range: bytes */N` header in the error response.
    RangeNotSatisfiable {
        /// The total size of the object in bytes.
        complete_length: u64,
    },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { key } => write!(f, "object not found: {key}"),
            Self::BucketNotFound { bucket } => write!(f, "bucket not found: {bucket}"),
            Self::BackendUnavailable { message } => write!(f, "backend unavailable: {message}"),
            Self::Other { source } => write!(f, "storage error: {source}"),
            Self::NotModified => write!(f, "not modified"),
            Self::PreconditionFailed => write!(f, "precondition failed"),
            Self::RangeNotSatisfiable { complete_length } => {
                write!(f, "range not satisfiable (object size: {complete_length})")
            }
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Other { source } = self {
            Some(source.as_ref())
        } else {
            None
        }
    }
}

impl StorageError {
    /// Returns `true` if this error represents a missing object or bucket.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. } | Self::BucketNotFound { .. })
    }
}

impl From<StorageError> for Error {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotModified => Error::NotModified,
            StorageError::PreconditionFailed => Error::PreconditionFailed,
            StorageError::RangeNotSatisfiable { complete_length } => {
                Error::RangeNotSatisfiable { complete_length }
            }
            other => Error::Storage(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_invalid_input_display() {
        let e = Error::InvalidInput {
            message: "bad".into(),
        };
        let s = e.to_string();
        assert!(s.contains("bad"), "Display should contain the message: {s}");
    }

    #[test]
    fn test_error_payload_too_large_display() {
        let e = Error::PayloadTooLarge {
            size: 17_000_000_u64,
            limit: 16_777_216_u64,
        };
        let s = e.to_string();
        assert!(
            s.contains("17000000") || s.contains("17_000_000"),
            "Display should contain size: {s}"
        );
        assert!(
            s.contains("16777216") || s.contains("16_777_216"),
            "Display should contain limit: {s}"
        );
    }

    #[test]
    fn test_storage_error_not_found_display() {
        let e = StorageError::NotFound { key: "foo".into() };
        let s = e.to_string();
        assert!(s.contains("foo"), "Display should contain the key: {s}");
    }

    #[test]
    fn test_storage_error_is_not_found_true_for_not_found() {
        let e = StorageError::NotFound { key: "bar".into() };
        assert!(e.is_not_found());
    }

    #[test]
    fn test_storage_error_is_not_found_true_for_bucket_not_found() {
        let e = StorageError::BucketNotFound {
            bucket: "my-bucket".into(),
        };
        assert!(e.is_not_found());
    }

    #[test]
    fn test_storage_error_is_not_found_false_for_backend_unavailable() {
        let e = StorageError::BackendUnavailable {
            message: "connection refused".into(),
        };
        assert!(!e.is_not_found());
    }

    #[test]
    fn test_from_storage_error_for_error() {
        let se = StorageError::NotFound { key: "obj".into() };
        let e: Error = Error::from(se);
        assert!(matches!(e, Error::Storage(_)));
    }

    #[test]
    fn test_error_implements_std_error() {
        fn assert_std_error<T: std::error::Error>(_: &T) {}
        let e = Error::InvalidInput {
            message: "test".into(),
        };
        assert_std_error(&e);
    }

    #[test]
    fn test_storage_error_implements_std_error() {
        fn assert_std_error<T: std::error::Error>(_: &T) {}
        let e = StorageError::BucketNotFound { bucket: "b".into() };
        assert_std_error(&e);
    }

    #[test]
    fn test_error_not_found_display() {
        let e = Error::NotFound {
            resource: "kb:my-notes".into(),
        };
        let s = e.to_string();
        assert!(
            s.contains("my-notes"),
            "Display should contain resource: {s}"
        );
    }

    #[test]
    fn test_error_bucket_name_too_long_fields() {
        let e = Error::BucketNameTooLong {
            name: "nt-toolong-name".into(),
            len: 15_usize,
        };
        if let Error::BucketNameTooLong { name, len } = &e {
            assert_eq!(name, "nt-toolong-name");
            assert_eq!(*len, 15);
        } else {
            panic!("Wrong variant");
        }
    }

    #[test]
    fn storage_error_range_not_satisfiable_converts_to_error() {
        let storage_err = StorageError::RangeNotSatisfiable {
            complete_length: 100,
        };
        let err: Error = Error::from(storage_err);
        assert!(matches!(
            err,
            Error::RangeNotSatisfiable {
                complete_length: 100
            }
        ));
    }

    #[test]
    fn storage_error_not_modified_converts_to_error() {
        let storage_err = StorageError::NotModified;
        let err: Error = Error::from(storage_err);
        assert!(matches!(err, Error::NotModified));
    }

    #[test]
    fn storage_error_precondition_failed_converts_to_error() {
        let storage_err = StorageError::PreconditionFailed;
        let err: Error = Error::from(storage_err);
        assert!(matches!(err, Error::PreconditionFailed));
    }
}
