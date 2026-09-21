//! Searcher trait and implementation for hybrid search.
//!
//! See SPECIFICATIONS.md §6.10 (search API), §6.11 (crate dependency rules),
//! §8.5 (RRF fusion), §8.6 (filter selectivity mitigation), D56 (fetch
//! window and hit order).

pub(crate) mod filter;
mod hybrid;
mod preview;

pub use filter::{PostFilter, TranslatedFilter, translate_filter};
pub use hybrid::HybridSearcher;
pub use preview::{PREVIEW_MAX_CHARS, truncate_preview};

use async_trait::async_trait;
use notedthat_core::KbSlug;
use notedthat_core::search::{SearchError, SearchResponse, ValidatedRequest};

/// A caller-supplied predicate over object keys.
///
/// The searcher applies it in the same pass as the request's
/// `object_key_prefix`, before the fused window is truncated to `limit`, so a
/// narrow grant is served from the over-fetched window rather than from an
/// already-truncated page (#68, D56). The searcher knows nothing about who is
/// asking or why a key is refused; that stays with the caller.
pub type KeyPredicate<'a> = &'a (dyn Fn(&str) -> bool + Sync + 'a);

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
    /// `key_filter`, when given, decides which object keys may appear in the
    /// response. An implementation must apply it — together with the
    /// request's own `object_key_prefix` — before truncating to the request's
    /// `limit`, so that a page is filled from the whole candidate window and
    /// not shortened by keys the caller may not see. `None` means every key
    /// is acceptable.
    ///
    /// Returns `SearchError::UnknownKb` if the collection does not exist in Qdrant.
    /// Returns `SearchError::BackendUnavailable` if Qdrant or the embedder is unreachable.
    async fn search(
        &self,
        kb: &KbSlug,
        request: ValidatedRequest,
        key_filter: Option<KeyPredicate<'_>>,
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
