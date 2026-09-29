//! Heading-aware markdown chunker.
//!
//! [`stream_chunks`] splits markdown read from a seekable source into chunks at
//! H1/H2/H3 boundaries, holding only a bounded window in memory. Sections longer
//! than the character bound are split at a paragraph or whitespace break near the
//! bound, else at the bound itself. Byte offsets are ABSOLUTE in the source input.
//!
//! Sections shorter than a quarter of the character bound are merged with the
//! sections after them, and a short final chunk into the one before it, so no
//! chunk is only whitespace and only the last chunk can be shorter than that
//! minimum (D72).
//!
//! This low-level chunker treats frontmatter as raw Markdown; OKF indexing passes
//! the body through it and translates offsets back into the original document.
//!
mod streaming;

pub use streaming::{ChunkIter, stream_chunks};

/// Soft character cap per chunk (~800 tokens). Configurable in a later milestone.
pub const SOFT_CHAR_CAP: usize = 3_000;

/// A single chunk of a source markdown document.
///
/// Invariant: `raw_bytes[byte_start..byte_end]` reconstructs `text` exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Chunk text (a slice of the raw input).
    pub text: String,
    /// Absolute byte offset in the raw input where this chunk begins (inclusive).
    pub byte_start: usize,
    /// Absolute byte offset in the raw input where this chunk ends (exclusive).
    pub byte_end: usize,
    /// Markdown heading path from H1 → H3, e.g. `["Introduction", "Motivation"]`.
    /// Empty when the chunk precedes any heading.
    pub heading_path: Vec<String>,
}
