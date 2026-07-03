//! Search types for the `NotedThat` search API.
//!
//! This module provides the typed request/response surface for
//! `POST /v1/knowledgebases/{kb_slug}/search`.
//!
//! See SPECIFICATIONS.md §6.10 (SearchRequest/SearchFilter fields),
//! §6.13 (route contract), D41 (top-k only, limit clamping),
//! D43 (error mapping), D44 (preview truncation).

mod object_key;
pub use object_key::ObjectKey;

mod filter;
pub use filter::SearchFilter;

mod error;
pub use error::SearchError;

mod hit;
pub use hit::SearchHit;

mod response;
pub use response::SearchResponse;

mod request;
pub use request::{
    DEFAULT_LIMIT, MAX_LIMIT, MAX_QUERY_BYTES, MIN_LIMIT, SearchRequest, ValidatedRequest,
};

// TODO(m5): populated by T3-T8
