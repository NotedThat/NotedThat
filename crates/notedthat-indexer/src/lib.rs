//! Async indexing pipeline for NotedThat.
//!
//! See SPECIFICATIONS.md §6.2, §6.3, §6.4, §6.11, §6.12 for context.

pub mod chunker;
pub mod embedder;
pub mod event;
pub mod provisioner;
pub mod qdrant;
pub mod worker;
