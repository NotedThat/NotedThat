use std::io::Cursor;

use notedthat_indexer::chunker::{Chunk, stream_chunks};

fn merged(raw: &str, max_chars: usize) -> Vec<Chunk> {
    let chunks = stream_chunks(Cursor::new(raw.as_bytes()), max_chars, 0)
        .expect("valid chunk iterator")
        .collect::<std::io::Result<Vec<_>>>()
        .expect("valid UTF-8 input");
    assert_invariants(raw, &chunks, max_chars);
    chunks
}

/// Every chunk is an exact, bounded, non-blank slice, in order; together they
/// cover the input except for whitespace that had nothing to merge into. Only
/// the last chunk may be shorter than the minimum (D72).
fn assert_invariants(raw: &str, chunks: &[Chunk], max_chars: usize) {
    let mut covered = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
        let chars = chunk.text.chars().count();
        assert!(chars <= max_chars, "{chunk:?}");
        assert!(
            index + 1 == chunks.len() || chars >= max_chars / 4,
            "short chunk before the end {chunk:?} in {raw:?}"
        );
        assert!(!chunk.text.trim().is_empty(), "blank chunk {chunk:?}");
        assert!(raw[covered..chunk.byte_start].trim().is_empty(), "{raw:?}");
        covered = chunk.byte_end;
    }
    assert!(raw[covered..].trim().is_empty(), "{raw:?}");
}

fn paragraph(prefix: &str, chars: usize) -> String {
    let mut text = format!("{prefix} ");
    while text.chars().count() + 5 < chars {
        text.push_str("word ");
    }
    text.push_str("\n\n");
    text
}

#[test]
fn heading_only_sections_merge_into_the_section_with_content() {
    // Given
    let raw = "# A\n## B\n### C\nbody";

    // When
    let chunks = merged(raw, 3_000);

    // Then
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, raw);
    assert_eq!(chunks[0].heading_path, ["A"]);
}

#[test]
fn heading_only_section_merges_forward_with_its_first_subsection() {
    // Given
    let raw = format!(
        "# RFC 9110\n\n## 1. Introduction\n\n{}",
        paragraph("HTTP is", 900)
    );

    // When
    let chunks = merged(&raw, 3_000);

    // Then
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].heading_path, ["RFC 9110"]);
}

#[test]
fn short_parent_stays_with_the_short_child_that_holds_its_answer() {
    // Given
    let parent = paragraph("# html.global_attributes.popover\n\nspec_url:", 630);
    let child = paragraph(
        "## Browser and runtime support\n\n- Safari: supported since 17\n",
        1_200,
    );
    let raw = format!("{parent}{child}");

    // When
    let chunks = merged(&raw, 3_000);

    // Then
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].text.contains("Safari: supported since 17"));
    assert_eq!(chunks[0].heading_path, ["html.global_attributes.popover"]);
}

#[test]
fn short_parent_takes_a_prefix_of_a_long_child_and_the_rest_keeps_the_child_path() {
    // Given
    let child: String = (0..10).map(|_| paragraph("child", 700)).collect();
    let raw = format!("# Parent\n\nshort intro.\n\n## Child\n\n{child}");

    // When
    let chunks = merged(&raw, 3_000);

    // Then
    assert!(chunks.len() > 2);
    assert_eq!(chunks[0].byte_start, 0);
    assert_eq!(chunks[0].heading_path, ["Parent"]);
    assert!(chunks[0].text.contains("## Child\n\nchild"));
    assert!(
        chunks[1..]
            .iter()
            .all(|chunk| chunk.heading_path == ["Parent", "Child"])
    );
}

#[test]
fn soft_cap_remainder_is_rebalanced_to_the_minimum() {
    // Given
    let raw = "word ".repeat(602);

    // When
    let chunks = merged(&raw, 3_000);

    // Then
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|chunk| chunk.text.chars().count() >= 750));
}

#[test]
fn short_final_section_merges_into_the_chunk_before_it() {
    // Given
    let raw = format!(
        "## Teapot\n\n{}## Status\n\n```http\n418 I'm a teapot\n```\n   ",
        paragraph("I refuse", 2_000)
    );

    // When
    let chunks = merged(&raw, 3_000);

    // Then
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].byte_end, raw.len());
    assert_eq!(chunks[0].heading_path, ["Teapot"]);
}

#[test]
fn four_byte_remainder_past_the_read_ahead_merges_forward() {
    // Given: 47 characters in 176 bytes, more than the split's 160-byte
    // read-ahead at a 40-character cap, so the first split cannot see the end.
    let section = format!("# A\n{}", "🙂".repeat(43));
    let followed = format!("{section}## B\nshort");

    // When
    let alone = merged(&section, 40);
    let merged_forward = merged(&followed, 40);

    // Then: the short remainder is a chunk of its own only at the end.
    assert_eq!(
        alone
            .iter()
            .map(|chunk| chunk.text.chars().count())
            .collect::<Vec<_>>(),
        [40, 7]
    );
    assert_eq!(merged_forward.len(), 2);
    assert_eq!(
        merged_forward[1].text,
        format!("{}## B\nshort", "🙂".repeat(7))
    );
    assert_eq!(merged_forward[1].heading_path, ["A"]);
}

#[test]
fn trailing_heading_that_does_not_fit_the_chunk_before_it_stays_on_its_own() {
    // Given
    let raw = format!("# A\n{}\n## Tail\n", "y".repeat(34));

    // When
    let chunks = merged(&raw, 40);

    // Then
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[1].text, "## Tail\n");
    assert_eq!(chunks[1].heading_path, ["A", "Tail"]);
}

#[test]
fn blank_preamble_takes_the_path_of_the_first_heading() {
    // Given
    let raw = format!("\r\n\r\n# Title\r\n\r\n{}", paragraph("body", 1_000));

    // When
    let chunks = merged(&raw, 3_000);

    // Then
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].byte_start, 0);
    assert_eq!(chunks[0].heading_path, ["Title"]);
}

#[test]
fn blank_documents_yield_nothing_and_a_lone_heading_is_kept() {
    // Given
    let blank = " \n\n\t\r\n  ";
    let heading = "# Title\n";

    // When
    let blank_chunks = merged(blank, 3_000);
    let heading_chunks = merged(heading, 3_000);

    // Then
    assert!(blank_chunks.is_empty());
    assert_eq!(heading_chunks.len(), 1);
    assert_eq!(heading_chunks[0].text, heading);
}

#[test]
fn invariants_hold_for_generated_markdown_at_small_and_default_bounds() {
    // Given
    const PIECES: [&str; 11] = [
        "# H1\n",
        "## H2\n",
        "### H3\n",
        "\n",
        "\r\n\r\n",
        "   ",
        "word ",
        "🙂 日本語 ",
        "🙂🙂🙂🙂🙂🙂",
        "Setext\n====\n",
        "```\n# fenced\n```\n",
    ];
    let mut state = 0x2545_f491_u32;
    let documents: Vec<String> = (0..200)
        .map(|_| {
            let pieces = 1 + state % 400;
            (0..pieces)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    PIECES[state as usize % PIECES.len()]
                })
                .collect()
        })
        .collect();

    for max_chars in [5, 7, 64, 3_000] {
        for raw in &documents {
            // When / Then
            merged(raw, max_chars);
        }
    }
}
