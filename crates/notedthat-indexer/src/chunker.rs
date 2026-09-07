//! Heading-aware markdown chunker.
//!
//! Splits raw markdown into chunks at H1/H2/H3 boundaries with a soft char cap.
//! Byte offsets are ABSOLUTE in the source input.
//!
//! Per §6.3 / D15: uses `pulldown-cmark::into_offset_iter` for byte offsets and
//! `text-splitter::MarkdownSplitter` as the secondary splitter for oversized sections.
//!
//! Frontmatter is treated as raw markdown per D33, except for OKF-conformant
//! documents: [`chunk_from`] lets the caller start heading discovery past a
//! frontmatter block, whose metadata is indexed separately (D48).
//!
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use text_splitter::MarkdownSplitter;

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
pub fn chunk(raw: &str) -> Vec<Chunk> {
    chunk_from(raw, 0)
}

/// Chunk `raw`, skipping the first `body_start` bytes.
///
/// Heading discovery runs only over `raw[body_start..]`, so a YAML frontmatter
/// block can no longer be mis-parsed as a setext heading and leak into every
/// chunk's `heading_path`. Byte offsets stay **absolute in `raw`**, which is what
/// keeps D6 intact: a search hit still dereferences to an exact byte range of the
/// stored object, and the round-trip invariant
/// `raw[chunk.byte_start..chunk.byte_end] == chunk.text` still holds because
/// chunks are sliced out of `raw` rather than out of a substring.
///
/// The skipped preamble is not chunked. For an OKF concept it is indexed as its
/// own metadata point instead (D48); `chunk_from(raw, 0)` is exactly [`chunk`].
///
/// `body_start` is clamped to the input length and floored to a character
/// boundary, so this never panics.
pub fn chunk_from(raw: &str, body_start: usize) -> Vec<Chunk> {
    if raw.is_empty() {
        return Vec::new();
    }
    let body_start = floor_char_boundary(raw, body_start.min(raw.len()));

    let mut boundaries = heading_boundaries(&raw[body_start..]);
    for boundary in &mut boundaries {
        boundary.start_byte += body_start;
    }

    if boundaries.is_empty() {
        return split_section(raw, body_start, raw.len(), &[]);
    }

    let mut chunks = Vec::new();
    let mut heading_path = Vec::new();
    let mut section_start_byte = body_start;

    for boundary in boundaries {
        chunks.extend(split_section(
            raw,
            section_start_byte,
            boundary.start_byte,
            &heading_path,
        ));

        let keep = boundary.depth.saturating_sub(1);
        heading_path.truncate(keep.min(heading_path.len()));
        heading_path.push(boundary.text);
        section_start_byte = boundary.start_byte;
    }

    chunks.extend(split_section(
        raw,
        section_start_byte,
        raw.len(),
        &heading_path,
    ));
    chunks
}

/// The largest character boundary at or below `index`.
///
/// `str::floor_char_boundary` is still unstable, and a caller-supplied offset
/// must never be able to panic the indexer.
fn floor_char_boundary(raw: &str, index: usize) -> usize {
    let mut i = index.min(raw.len());
    while i > 0 && !raw.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[derive(Debug)]
struct HeadingBoundary {
    start_byte: usize,
    depth: usize,
    text: String,
}

fn heading_boundaries(raw: &str) -> Vec<HeadingBoundary> {
    let mut boundaries = Vec::new();
    let mut active_heading: Option<HeadingBoundary> = None;

    for (event, range) in Parser::new(raw).into_offset_iter() {
        #[allow(clippy::match_same_arms)]
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active_heading = Some(HeadingBoundary {
                    start_byte: range.start,
                    depth: heading_depth(level),
                    text: String::new(),
                });
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(heading) = active_heading.as_mut() {
                    heading.text.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(mut heading) = active_heading.take() {
                    heading.text = heading.text.trim().to_string();
                    boundaries.push(heading);
                }
            }
            Event::Rule | Event::HardBreak | Event::SoftBreak => {}
            _ => {}
        }
    }

    boundaries
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => 3,
    }
}

fn split_section(raw: &str, start: usize, end: usize, heading_path: &[String]) -> Vec<Chunk> {
    if start >= end {
        return Vec::new();
    }

    let Some(section_text) = raw.get(start..end) else {
        return Vec::new();
    };

    if section_text.chars().count() <= SOFT_CHAR_CAP {
        return vec![make_chunk(raw, start, end, heading_path)];
    }

    let splitter = MarkdownSplitter::new(SOFT_CHAR_CAP);
    splitter
        .chunk_indices(section_text)
        .filter_map(|(offset, text)| {
            let relative_start = byte_offset_for_split(section_text, offset, text)?;
            let byte_start = start + relative_start;
            let byte_end = byte_start + text.len();
            if byte_start >= byte_end {
                return None;
            }
            Some(make_chunk(raw, byte_start, byte_end, heading_path))
        })
        .collect()
}

fn byte_offset_for_split(section_text: &str, offset: usize, split_text: &str) -> Option<usize> {
    if section_text.get(offset..offset.saturating_add(split_text.len())) == Some(split_text) {
        return Some(offset);
    }

    let byte_offset = section_text
        .char_indices()
        .nth(offset)
        .map(|(byte_offset, _)| byte_offset)
        .or_else(|| (offset == section_text.chars().count()).then_some(section_text.len()))?;

    (section_text.get(byte_offset..byte_offset.saturating_add(split_text.len()))
        == Some(split_text))
    .then_some(byte_offset)
}

fn make_chunk(raw: &str, byte_start: usize, byte_end: usize, heading_path: &[String]) -> Chunk {
    Chunk {
        text: raw[byte_start..byte_end].to_string(),
        byte_start,
        byte_end,
        heading_path: heading_path.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        assert!(chunk("").is_empty());
    }

    #[test]
    fn no_headings_one_chunk() {
        let chunks = chunk("hello");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, Vec::<String>::new());
    }

    #[test]
    fn h1_heading() {
        let chunks = chunk("# H1\n\nbody");
        assert!(chunks.iter().any(|c| c.heading_path == vec!["H1"]));
    }

    #[test]
    fn h2_without_h1() {
        let chunks = chunk("## B\nx");
        assert!(chunks.iter().any(|c| c.heading_path == vec!["B"]));
    }

    #[test]
    fn nested_headings() {
        let raw = "# A\ntop\n## B\nsub\n### C\nsub2";
        let chunks = chunk(raw);
        let paths: Vec<_> = chunks.iter().map(|c| c.heading_path.clone()).collect();
        assert!(paths.contains(&vec!["A".to_string()]));
        assert!(paths.contains(&vec!["A".to_string(), "B".to_string()]));
        assert!(paths.contains(&vec!["A".to_string(), "B".to_string(), "C".to_string()]));
    }

    #[test]
    fn sibling_headings() {
        let chunks = chunk("# A\n## B\n## C");
        let paths: Vec<_> = chunks.iter().map(|c| c.heading_path.clone()).collect();
        assert!(paths.iter().any(|p| p == &vec!["A", "B"]));
        assert!(paths.iter().any(|p| p == &vec!["A", "C"]));
    }

    #[test]
    fn round_trip_invariant() {
        let raw = "# A\n\nsome body text\n\n## B\n\nmore body\n\n### C\n\ndeep section";
        let chunks = chunk(raw);
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(
                &raw[c.byte_start..c.byte_end],
                c.text.as_str(),
                "round-trip failed for chunk with heading_path={:?}",
                c.heading_path
            );
        }
    }

    #[test]
    fn no_empty_chunks() {
        let chunks = chunk("# A\n## B");
        for c in &chunks {
            assert!(c.byte_start < c.byte_end, "empty chunk found: {c:?}");
        }
    }

    #[test]
    fn utf8_heading() {
        let raw = "# 日本語\n本文";
        let chunks = chunk(raw);
        assert!(!chunks.is_empty());
        let heading_chunk = chunks.iter().find(|c| !c.heading_path.is_empty());
        assert_eq!(
            heading_chunk.map(|c| c.heading_path[0].as_str()),
            Some("日本語")
        );
        for c in &chunks {
            assert_eq!(&raw[c.byte_start..c.byte_end], c.text.as_str());
        }
    }

    #[test]
    fn no_panic_null_byte() {
        chunk("\0");
    }

    #[test]
    fn no_panic_deep_headings() {
        chunk("############# H");
    }

    #[test]
    fn no_panic_whitespace() {
        chunk("\n\n\n");
    }

    #[test]
    fn soft_cap_splits_large_section() {
        let body = "word ".repeat(1000);
        let raw = format!("# Title\n\n{body}");
        let chunks = chunk(&raw);
        assert!(
            chunks.len() >= 2,
            "expected split but got {} chunks",
            chunks.len()
        );
        for c in &chunks {
            assert_eq!(c.heading_path, vec!["Title"]);
        }
    }

    #[test]
    fn frontmatter_yaml() {
        let raw = "---\ntitle: t\n---\n# H\nbody";
        let chunks = chunk(raw);
        assert!(
            chunks.iter().any(|c| c.heading_path == vec!["H"]),
            "no H heading chunk found, paths: {:?}",
            chunks.iter().map(|c| &c.heading_path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunk_is_send_sync_clone_debug() {
        fn bounds<T: Send + Sync + Clone + std::fmt::Debug>() {}
        bounds::<Chunk>();
    }
    // --- chunk_from (D48) ---

    const OKF_DOC: &str = "---\ntype: Metric\ntitle: Daily Revenue\n---\n# Body\n\ntext here\n";

    /// `body_start` of `OKF_DOC`, i.e. just past the closing delimiter line.
    fn okf_body_start() -> usize {
        OKF_DOC.find("# Body").unwrap()
    }

    #[test]
    fn chunk_from_zero_is_exactly_chunk() {
        for raw in [
            "",
            "hello",
            "# H1\n\nbody",
            "## B\nx",
            OKF_DOC,
            "---\nnot: okf\n---\n# H\n",
        ] {
            assert_eq!(chunk_from(raw, 0), chunk(raw), "mismatch for {raw:?}");
        }
    }

    #[test]
    fn chunk_from_skips_the_preamble() {
        let chunks = chunk_from(OKF_DOC, okf_body_start());
        assert_eq!(chunks[0].byte_start, okf_body_start());
        assert!(!chunks.iter().any(|c| c.text.contains("type: Metric")));
    }

    #[test]
    fn chunk_from_keeps_offsets_absolute_in_the_input() {
        // The D6 invariant, asserted against the stored document.
        for chunk in chunk_from(OKF_DOC, okf_body_start()) {
            assert_eq!(&OKF_DOC[chunk.byte_start..chunk.byte_end], chunk.text);
        }
    }

    #[test]
    fn chunk_from_finds_body_headings() {
        let chunks = chunk_from(OKF_DOC, okf_body_start());
        assert!(chunks.iter().any(|c| c.heading_path == vec!["Body"]));
    }

    #[test]
    fn chunk_from_removes_the_phantom_setext_heading() {
        // `title: Daily Revenue` followed by the closing `---` parses as a setext
        // H2 when the frontmatter is included, polluting every heading_path.
        let polluted = chunk(OKF_DOC);
        assert!(
            polluted
                .iter()
                .any(|c| c.heading_path.iter().any(|h| h.contains("title:"))),
            "expected the pre-D48 phantom heading to be present"
        );
        let clean = chunk_from(OKF_DOC, okf_body_start());
        assert!(
            !clean
                .iter()
                .any(|c| c.heading_path.iter().any(|h| h.contains("title:"))),
            "phantom setext heading leaked into {clean:?}"
        );
    }

    #[test]
    fn chunk_from_a_frontmatter_only_document_yields_no_chunks() {
        // Correct: the metadata point carries this document, not a body chunk.
        let raw = "---\ntype: Metric\n---\n";
        assert!(chunk_from(raw, raw.len()).is_empty());
    }

    #[test]
    fn chunk_from_past_the_end_does_not_panic() {
        assert!(chunk_from("hello", 999).is_empty());
    }

    #[test]
    fn chunk_from_a_non_char_boundary_does_not_panic() {
        let raw = "\u{1f680}body";
        // 1 and 2 are inside the leading four-byte character.
        for offset in [1, 2, 3] {
            let chunks = chunk_from(raw, offset);
            for chunk in chunks {
                assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
            }
        }
    }

    #[test]
    fn chunk_from_an_empty_input_yields_nothing() {
        assert!(chunk_from("", 0).is_empty());
        assert!(chunk_from("", 10).is_empty());
    }

    #[test]
    fn chunk_from_splits_an_oversized_body_and_keeps_the_invariant() {
        let big = "x".repeat(SOFT_CHAR_CAP * 3);
        let raw = format!("---\ntype: Metric\n---\n# H\n\n{big}\n");
        let body_start = raw.find("# H").unwrap();
        let chunks = chunk_from(&raw, body_start);
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
            assert!(chunk.byte_start >= body_start);
        }
    }
}
