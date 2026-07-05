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
}

impl From<StorageError> for WriteError {
    fn from(err: StorageError) -> Self {
        Self::Storage(err)
    }
}
