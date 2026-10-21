//! Domain error types: `Error`, `StorageError` and the wire `ErrorCode`.

use crate::search::SearchError;
use serde::Serialize;
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

    /// A storage-layer error (see [`StorageError`]). The one spelling of every
    /// storage condition, 304/412/416 included.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// Malformed `Range: bytes=` header — unparseable syntax. Maps to HTTP 400.
    #[error("malformed Range header: {0}")]
    MalformedRange(String),
}

/// The validators of the object a 304 Not Modified refers to.
///
/// RFC 9110 §15.4.5: a 304 must carry the `ETag` and `Last-Modified` a 200 for the same
/// request would have. Either can be absent when the backend did not report it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    /// The object's `ETag`, quoted per RFC 9110 §8.8.3.
    pub etag: Option<String>,
    /// The object's last modification time, in Unix seconds.
    pub last_modified: Option<i64>,
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

    /// The read's preconditions answered 304 Not Modified. Carries the object's
    /// validators, which the 304 must repeat (RFC 9110 §15.4.5).
    NotModified(Validators),

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
            Self::NotModified(_) => write!(f, "not modified"),
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

impl From<SearchError> for Error {
    fn from(e: SearchError) -> Self {
        match e {
            SearchError::InvalidInput { message } => Error::InvalidInput { message },
            SearchError::UnknownKb { slug } => Error::NotFound {
                resource: format!("knowledgebase '{slug}'"),
            },
            SearchError::BackendUnavailable { message } => {
                Error::Storage(StorageError::BackendUnavailable { message })
            }
            SearchError::Internal { message } => Error::Config { message },
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
}

/// The machine-readable `error` of the JSON envelope (D43, §6.12), shared by every
/// surface so the codes are spelled once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(
    feature = "openapi",
    schema(
        as = ErrorCode,
        description = "Machine-readable error code. Clients branch on this, never on `message`."
    )
)]
pub enum ErrorCode {
    /// The request is malformed: a bad path, body, header or parameter.
    InvalidRequest,
    /// The `Range` header could not be parsed.
    MalformedRange,
    /// No credential, or one that did not verify.
    Unauthorized,
    /// The credential is valid and the access policy says no.
    Forbidden,
    /// No such knowledge base, object or action — or one the caller may not see.
    NotFound,
    /// The request conflicts with work in progress.
    Conflict,
    /// The events requested are no longer retained.
    Gone,
    /// The body exceeds the route's limit.
    PayloadTooLarge,
    /// A conditional request's precondition did not hold.
    PreconditionFailed,
    /// A replace found no occurrence of `old_string`.
    NoMatch,
    /// A replace found more than one occurrence of `old_string`.
    AmbiguousMatch,
    /// A write that must be conditional arrived without `If-Match`.
    PreconditionRequired,
    /// The server failed.
    InternalError,
    /// A backend or queue is unavailable or full; retry after `Retry-After`.
    BackendUnavailable,
    /// The request body stalled, or the request exceeded the server's timeout.
    RequestTimeout,
    /// Internal only: a `416` is answered with an empty body, so this code is
    /// never written. Skipped so the published enum lists only what is sent.
    #[serde(skip)]
    RangeNotSatisfiable,
    /// Internal only: a `304` has no body, so this code is never written.
    #[serde(skip)]
    NotModified,
}

impl ErrorCode {
    /// Every code, the internal ones included.
    pub const ALL: [Self; 17] = [
        Self::InvalidRequest,
        Self::MalformedRange,
        Self::Unauthorized,
        Self::Forbidden,
        Self::NotFound,
        Self::Conflict,
        Self::Gone,
        Self::PayloadTooLarge,
        Self::PreconditionFailed,
        Self::NoMatch,
        Self::AmbiguousMatch,
        Self::PreconditionRequired,
        Self::InternalError,
        Self::BackendUnavailable,
        Self::RequestTimeout,
        Self::RangeNotSatisfiable,
        Self::NotModified,
    ];

    /// The spelling, for the internal codes as well as the ones on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::MalformedRange => "malformed_range",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Gone => "gone",
            Self::PayloadTooLarge => "payload_too_large",
            Self::PreconditionFailed => "precondition_failed",
            Self::NoMatch => "no_match",
            Self::AmbiguousMatch => "ambiguous_match",
            Self::PreconditionRequired => "precondition_required",
            Self::InternalError => "internal_error",
            Self::BackendUnavailable => "backend_unavailable",
            Self::RequestTimeout => "request_timeout",
            Self::RangeNotSatisfiable => "range_not_satisfiable",
            Self::NotModified => "not_modified",
        }
    }

    /// Whether the code is ever written into a body.
    #[must_use]
    pub fn is_sent(self) -> bool {
        !matches!(self, Self::RangeNotSatisfiable | Self::NotModified)
    }
}

impl PartialEq<&str> for ErrorCode {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
