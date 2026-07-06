//! Error types for the shared write path.

use notedthat_core::{Error as CoreError, StorageError};

/// Errors returned by shared write operations.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// Storage-layer failure.
    #[error(transparent)]
    Storage(StorageError),
    /// Upload size exceeded the configured limit.
    #[error("payload too large: {size} bytes (limit {limit})")]
    TooLarge {
        /// Actual byte size.
        size: u64,
        /// Maximum allowed byte size.
        limit: u64,
    },
    /// Path/domain validation failure.
    #[error(transparent)]
    Path(CoreError),
    /// Indexer queue was full while enqueueing an upsert.
    #[error("indexer queue full during upsert")]
    IndexerBackpressureUpsert,
    /// Indexer queue was full while enqueueing a tombstone.
    #[error("indexer queue full during tombstone")]
    IndexerBackpressureTombstone,
    /// Object body exceeds the NOTEDTHAT_MAX_PATCHABLE_SIZE limit before or after splice.
    #[error("patch payload too large: {size} bytes (limit {limit})")]
    PatchTooLarge {
        /// Actual byte size.
        size: u64,
        /// Maximum allowed byte size.
        limit: u64,
    },
    /// Requested line range is beyond the end of the object at server-side splice time.
    #[error("line range {first}..{last} out of range (total {total_lines} lines)")]
    PatchLineOutOfRange {
        /// Requested first line.
        first: u64,
        /// Requested last line.
        last: u64,
        /// Total line count in the object.
        total_lines: u64,
        /// Total byte count in the object (for X-Content-Range-Bytes).
        total_bytes: u64,
    },
    /// Invalid range, mode contradiction, or missing If-Match for PATCH.
    #[error("invalid patch request: {message}")]
    PatchInvalidRange {
        /// Human-readable reason.
        message: String,
    },
}

impl From<StorageError> for WriteError {
    fn from(err: StorageError) -> Self {
        Self::Storage(err)
    }
}

#[cfg(test)]
mod tests {
    use super::WriteError;

    #[test]
    fn patch_too_large_displays_size_limit_when_constructed() {
        let err = WriteError::PatchTooLarge {
            size: 200 * 1024 * 1024,
            limit: 100 * 1024 * 1024,
        };

        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn patch_line_out_of_range_constructs_with_line_and_byte_totals() {
        let err = WriteError::PatchLineOutOfRange {
            first: 999,
            last: 1000,
            total_lines: 20,
            total_bytes: 100,
        };

        assert!(err.to_string().contains("out of range"));
    }

    #[test]
    fn patch_invalid_range_displays_reason_when_constructed() {
        let err = WriteError::PatchInvalidRange {
            message: "test".into(),
        };

        assert!(err.to_string().contains("invalid patch"));
    }
}
