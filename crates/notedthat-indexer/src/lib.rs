//! Async indexing pipeline for `NotedThat`.
//!
//! See SPECIFICATIONS.md §6.2, §6.3, §6.4, §6.11, §6.12 for context.

pub mod chunker;
pub mod embedder;
pub mod event;
pub mod health;
pub mod okf;
pub mod provisioner;
pub mod qdrant;
pub mod searcher;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod vector_store;
pub mod worker;

pub use chunker::{Chunk, SOFT_CHAR_CAP, chunk};
pub use embedder::{Embedder, EmbedderError};
pub use embedder::{OpenAiCompatibleConfig, OpenAiCompatibleEmbedder};
pub use event::{IndexEvent, RefreshOrigin};
pub use health::{
    BACKPRESSURE_WINDOW, IndexFailure, IndexHealth, IndexState, KbHealthSnapshot, ReconcileSummary,
};
pub use provisioner::{ProvisionError, QdrantProvisioner};
pub use qdrant::{QdrantClient, QdrantConfig, QdrantWrapperError};
pub use searcher::{KeyPredicate, Searcher};
pub use vector_store::{
    HybridQuery, IndexedObject, PayloadFieldKind, PointSelector, VectorStore, VectorStoreError,
};
pub use worker::{DRAIN_TIMEOUT, IndexerWorker};
