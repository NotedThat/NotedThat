//! Searcher trait and implementation for hybrid search.
//!
//! See SPECIFICATIONS.md §6.10 (search API), §6.11 (crate dependency rules),
//! §9.5 (RRF fusion), §9.6 (filter selectivity mitigation).

mod preview;

pub use preview::{truncate_preview, PREVIEW_MAX_CHARS};

use async_trait::async_trait;
use notedthat_core::KbSlug;
use notedthat_core::search::{SearchError, SearchResponse, ValidatedRequest};

/// Performs hybrid search against a Qdrant collection.
///
/// The concrete implementation is `HybridSearcher`. This trait allows
/// the HTTP layer to accept `Arc<dyn Searcher>` for test injection.
///
/// Both `KbSlug` and `ValidatedRequest` are pre-validated — callers cannot
/// bypass validation at the trait boundary.
#[async_trait]
pub trait Searcher: Send + Sync {
    /// Search the given knowledge base with the validated request.
    ///
    /// Returns `SearchError::UnknownKb` if the collection does not exist in Qdrant.
    /// Returns `SearchError::BackendUnavailable` if Qdrant or the embedder is unreachable.
    async fn search(
        &self,
        kb: &KbSlug,
        request: ValidatedRequest,
    ) -> Result<SearchResponse, SearchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searcher_trait_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn Searcher>();
    }
}
