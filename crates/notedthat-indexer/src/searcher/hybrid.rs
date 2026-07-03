//! Hybrid searcher combining dense (cosine) + sparse (BM25) prefetches
//! with Qdrant's server-side RRF fusion.

use std::sync::Arc;
use notedthat_core::KbSlug;
use crate::qdrant::QdrantClient;
use crate::embedder::Embedder;
use crate::worker::collection_name;

/// Hybrid searcher that combines dense (cosine) + sparse (BM25) prefetches
/// with Qdrant's server-side RRF fusion.
///
/// The same `QdrantClient` and `Embedder` instances used by `IndexerWorker`
/// are shared here — using separate instances would risk vector space mismatch
/// (§6.4, D18).
#[allow(dead_code)]
pub struct HybridSearcher {
    qdrant: Arc<QdrantClient>,
    embedder: Arc<dyn Embedder>,
}

impl HybridSearcher {
    /// Create a new `HybridSearcher`.
    ///
    /// Must receive the SAME `qdrant` and `embedder` instances used by the
    /// `IndexerWorker` — different instances risk model or endpoint drift.
    pub fn new(qdrant: Arc<QdrantClient>, embedder: Arc<dyn Embedder>) -> Self {
        Self { qdrant, embedder }
    }

    /// Returns the Qdrant collection name for the given knowledge base.
    #[allow(dead_code)]
    pub(crate) fn collection_for(kb: &KbSlug) -> String {
        collection_name(kb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_searcher_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HybridSearcher>();
    }

    #[test]
    fn collection_for_format() {
        use notedthat_core::KbSlug;
        let slug = KbSlug::try_new("notes").unwrap();
        assert_eq!(HybridSearcher::collection_for(&slug), "kb_notes_v1");
    }
}
