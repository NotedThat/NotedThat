//! Heading-aware markdown chunker.
//!
//! Splits raw markdown into chunks at H1/H2/H3 boundaries with a soft char cap.
//! Byte offsets are ABSOLUTE in the source input.
//!
//! Per §6.3 / D15: uses `pulldown-cmark::into_offset_iter` for byte offsets and
//! `text-splitter::MarkdownSplitter` as the secondary splitter for oversized sections.
//!
//! Frontmatter is treated as raw markdown per D33.
//!
//! Real chunking implementation lands in T7. This module currently exposes only
//! the `Chunk` type and a stub `chunk()` function.

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

/// Chunk a raw markdown string.
///
/// Returns `Vec<Chunk>` where every chunk satisfies the round-trip invariant:
/// `raw[chunk.byte_start..chunk.byte_end] == chunk.text`.
///
/// The real heading-aware implementation lands in T7. This stub returns an empty
/// vec for empty input and a single whole-document chunk otherwise.
pub fn chunk(raw: &str) -> Vec<Chunk> {
    if raw.is_empty() {
        return Vec::new();
    }
    vec![Chunk {
        text: raw.to_string(),
        byte_start: 0,
        byte_end: raw.len(),
        heading_path: Vec::new(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty_vec() {
        assert!(chunk("").is_empty());
    }

    #[test]
    fn single_char_returns_one_chunk() {
        let chunks = chunk("x");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].byte_start, 0);
        assert_eq!(chunks[0].byte_end, 1);
        assert_eq!(chunks[0].text, "x");
        assert!(chunks[0].heading_path.is_empty());
    }

    #[test]
    fn round_trip_invariant_holds() {
        let raw = "Hello, world!";
        let chunks = chunk(raw);
        for c in &chunks {
            assert_eq!(&raw[c.byte_start..c.byte_end], c.text.as_str());
        }
    }

    #[test]
    fn chunk_is_send_sync_clone_debug() {
        fn _bounds<T: Send + Sync + Clone + std::fmt::Debug>() {}
        _bounds::<Chunk>();
    }
}
