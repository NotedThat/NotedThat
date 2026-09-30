//! Bounded streaming chunker behavior.

use std::io::Cursor;

use notedthat_indexer::chunker::{Chunk, stream_chunks};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

#[path = "streaming_chunker/bounds.rs"]
mod bounds;
#[path = "streaming_chunker/merging.rs"]
mod merging;

/// Collects chunks without merging short sections, so heading detection is
/// visible one section per chunk.
fn collect(raw: &str, max_chars: usize, base: usize) -> Vec<Chunk> {
    stream_chunks(Cursor::new(raw.as_bytes()), max_chars, base)
        .expect("valid chunk iterator")
        .with_min_chars(0)
        .collect::<std::io::Result<Vec<_>>>()
        .expect("valid UTF-8 input")
}

/// Section starts and heading paths as pulldown-cmark sees the whole document:
/// the oracle the streaming scanner's bounded view has to agree with.
fn parsed_sections(raw: &str) -> Vec<(usize, Vec<String>)> {
    let mut sections = Vec::new();
    let mut heading_path: Vec<String> = Vec::new();
    let mut active: Option<(usize, usize, String)> = None;
    for (event, range) in Parser::new(raw).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let depth = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                };
                active = Some((range.start, depth, String::new()));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, _, label)) = active.as_mut() {
                    label.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((start, depth, label)) = active.take() {
                    heading_path.truncate(depth - 1);
                    heading_path.push(label.trim().to_owned());
                    sections.push((start, heading_path.clone()));
                }
            }
            _ => {}
        }
    }
    if !raw.is_empty() && sections.first().is_none_or(|(start, _)| *start > 0) {
        sections.insert(0, (0, Vec::new()));
    }
    sections
}

#[test]
fn ranges_round_trip_with_absolute_base_and_unicode_boundaries() {
    // Given
    let raw = "intro 🙂\n\n# 日本語\nalpha beta gamma\n";
    let base = 37;

    // When
    let chunks = collect(raw, 7, base);

    // Then
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        raw
    );
    assert!(chunks.iter().all(|chunk| chunk.text.chars().count() <= 7));
    for chunk in &chunks {
        assert_eq!(
            &raw.as_bytes()[chunk.byte_start - base..chunk.byte_end - base],
            chunk.text.as_bytes()
        );
    }
    assert!(
        chunks
            .windows(2)
            .all(|pair| pair[0].byte_end == pair[1].byte_start)
    );
}

#[test]
fn finds_markers_at_read_buffer_edges_and_document_ends() {
    // Given
    let padding = "x".repeat(8_187);
    let raw = format!("# Begin\n{padding}\n## Middle\nbody\nTail\n====\nend");

    // When
    let chunks = collect(&raw, 3_000, 0);

    // Then
    let paths: Vec<_> = chunks
        .iter()
        .map(|chunk| chunk.heading_path.clone())
        .collect();
    assert!(paths.contains(&vec!["Begin".to_owned()]));
    assert!(paths.contains(&vec!["Begin".to_owned(), "Middle".to_owned()]));
    assert!(paths.contains(&vec!["bodyTail".to_owned()]));
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        raw
    );
}

#[test]
fn matches_bounded_parser_heading_boundaries_for_ordinary_markdown() {
    // Given
    let inputs = [
        "intro\n# A\nbody\n## B\nmore\n#### Deep\nend",
        "First line\nsecond *line*\n====\nbody\nNext\n----\n",
        "> # Quoted\n> body\n\n- ## Listed\n  body\n",
        "```\n# code\n```\n\n# real\n",
        "<article>\n# html\n</article>\n\n# real\n",
        "```bad`info\n# real\n",
    ];

    // When
    let actual: Vec<_> = inputs.iter().map(|raw| collect(raw, 100_000, 0)).collect();

    // Then
    for (raw, streamed) in inputs.iter().zip(actual) {
        let expected = parsed_sections(raw);
        let streamed: Vec<_> = streamed
            .into_iter()
            .map(|chunk| (chunk.byte_start, chunk.heading_path))
            .collect();
        assert_eq!(streamed, expected, "parser parity failed for {raw:?}");
    }
}

#[test]
fn handles_utf8_and_consecutive_headings_across_scanner_reads() {
    // Given
    let prefix = "a".repeat(8_190);
    let raw = format!("{prefix}🙂\n# A\n## B\n### C");

    // When
    let chunks = collect(&raw, 3_000, 0);

    // Then
    let starts: Vec<_> = chunks
        .iter()
        .filter(|chunk| !chunk.heading_path.is_empty())
        .map(|chunk| (chunk.byte_start, chunk.heading_path.clone()))
        .collect();
    assert_eq!(
        starts,
        [
            (prefix.len() + '🙂'.len_utf8() + 1, vec!["A".to_owned()]),
            (
                prefix.len() + '🙂'.len_utf8() + 5,
                vec!["A".to_owned(), "B".to_owned()]
            ),
            (
                prefix.len() + '🙂'.len_utf8() + 10,
                vec!["A".to_owned(), "B".to_owned(), "C".to_owned()]
            ),
        ]
    );
}

#[test]
fn closes_giant_html_and_container_fences_before_real_headings() {
    // Given
    let html = "x".repeat(40_000);
    let raw = format!("<script>{html}</script>\n# AfterHtml\n> ```\n> # code\n# AfterQuote\n");

    // When
    let chunks = collect(&raw, 3_000, 0);

    // Then
    let headings: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| chunk.heading_path.last().cloned())
        .collect();
    assert!(headings.contains(&"AfterHtml".to_owned()));
    assert!(headings.contains(&"AfterQuote".to_owned()));
    assert!(!headings.contains(&"code".to_owned()));
}

#[test]
fn giant_unicode_setext_and_indented_atx_keep_markdown_meaning() {
    // Given
    let title = "🙂".repeat(10_000);
    let indented = format!("    # {}\n", "not-a-heading".repeat(3_000));
    let raw = format!("{title}\n====\nbody\n{indented}# Real\n");

    // When
    let chunks = collect(&raw, 3_000, 0);

    // Then
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.text.chars().count() <= 3_000)
    );
    assert!(chunks.iter().any(|chunk| {
        chunk.heading_path.first().is_some_and(|heading| {
            heading.chars().count() == 3_000 && heading.chars().all(|character| character == '🙂')
        })
    }));
    assert!(chunks.iter().any(|chunk| {
        chunk
            .heading_path
            .last()
            .is_some_and(|heading| heading == "Real")
    }));
    assert!(!chunks.iter().any(|chunk| {
        chunk
            .heading_path
            .iter()
            .any(|heading| heading.starts_with("not-a-heading"))
    }));
}

#[test]
fn ignores_heading_syntax_in_fenced_indented_container_and_html_blocks() {
    // Given
    let raw = concat!(
        "# Real\n",
        "```markdown\n# fenced\n```\n",
        "    # indented\n",
        "> - nested text\n>   still nested\n",
        "<div>\n# html\n</div>\n\n",
        "> ## Nested\nbody\n",
        "> <div>\n> # quoted html\n# AfterQuoteHtml\n",
    );

    // When
    let chunks = collect(raw, 3_000, 0);

    // Then
    let paths: Vec<_> = chunks
        .iter()
        .map(|chunk| chunk.heading_path.clone())
        .collect();
    assert!(paths.contains(&vec!["Real".to_owned()]));
    assert!(paths.contains(&vec!["Real".to_owned(), "Nested".to_owned()]));
    assert!(paths.contains(&vec!["AfterQuoteHtml".to_owned()]));
    assert!(
        !paths
            .iter()
            .flatten()
            .any(|heading| { heading == "fenced" || heading == "indented" || heading == "html" })
    );
}

#[test]
fn caps_heading_labels_and_text_for_giant_lines_and_fences() {
    // Given
    let giant_heading = "h".repeat(40_000);
    let giant_fence = "z".repeat(100_000);
    let giant_word = "w".repeat(50_000);
    let raw = format!("# {giant_heading}\n```\n{giant_fence}\n```\n{giant_word}");

    // When
    let chunks = collect(&raw, 3_000, 0);

    // Then
    assert!(chunks.len() > 20);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.text.chars().count() <= 3_000)
    );
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.heading_path[0].chars().count() == 3_000)
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        raw
    );
}

#[test]
fn rejects_zero_bound_and_invalid_utf8() {
    // Given
    // When
    let zero = stream_chunks(Cursor::new(b"text"), 0, 0);
    let mut invalid = stream_chunks(Cursor::new(&b"valid\xfftail"[..]), 20, 0)
        .expect("non-zero bound is accepted");

    // Then
    assert_eq!(
        zero.err().expect("zero bound must fail").kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        invalid
            .next()
            .expect("one iterator result")
            .expect_err("invalid UTF-8 must fail")
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(invalid.next().is_none());
}

#[test]
fn preserves_headings_after_indented_html_like_code() {
    // Given
    let raw = "    <script>\n\n# Real\nbody\n";

    // When
    let chunks = collect(raw, 3_000, 0);

    // Then
    assert!(chunks.iter().any(|chunk| {
        chunk
            .heading_path
            .last()
            .is_some_and(|heading| heading == "Real")
    }));
}

#[test]
fn ends_unclosed_list_fences_when_the_list_ends() {
    // Given
    let raw = "- ```\n  code\n\n# Real\nbody\n";

    // When
    let chunks = collect(raw, 3_000, 0);

    // Then
    assert!(chunks.iter().any(|chunk| {
        chunk
            .heading_path
            .last()
            .is_some_and(|heading| heading == "Real")
    }));
}

#[test]
fn treats_html_tag_prefixes_as_ordinary_markdown() {
    // Given
    let raw = "<scripture>\n\n# Real\nbody\n";

    // When
    let chunks = collect(raw, 3_000, 0);

    // Then
    assert!(chunks.iter().any(|chunk| {
        chunk
            .heading_path
            .last()
            .is_some_and(|heading| heading == "Real")
    }));
}

#[test]
fn finds_html_terminators_anywhere_in_long_lines() {
    // Given
    let raw = format!(
        "<!--{}-->{}\n# Real\nbody\n",
        "x".repeat(17_000),
        "y".repeat(100)
    );

    // When
    let chunks = collect(&raw, 3_000, 0);

    // Then
    assert!(chunks.iter().any(|chunk| {
        chunk
            .heading_path
            .last()
            .is_some_and(|heading| heading == "Real")
    }));
}

fn paragraph() -> String {
    ["word"; 16].join(" ")
}

#[test]
fn crlf_and_lf_split_at_the_same_paragraph_breaks() {
    // Given
    let lf = vec![paragraph(); 4].join("\n\n");
    let crlf = lf.replace('\n', "\r\n");

    // When
    let lf_chunks = collect(&lf, 100, 0);
    let crlf_chunks = collect(&crlf, 100, 0);

    // Then
    assert_eq!(lf_chunks.len(), 4);
    assert_eq!(
        crlf_chunks
            .iter()
            .map(|chunk| chunk.text.replace("\r\n", "\n"))
            .collect::<Vec<_>>(),
        lf_chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>()
    );
    for (raw, chunks) in [(&lf, &lf_chunks), (&crlf, &crlf_chunks)] {
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.text.ends_with("\n\n") || chunk.text.ends_with("\r\n\r\n"));
        }
        for chunk in chunks {
            assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
        }
    }
}

#[test]
fn blank_lines_with_whitespace_or_mixed_endings_are_paragraph_breaks() {
    for separator in ["\n  \n", "\n\t\r\n", "\n\r\n", "\r\r", "\r\n \r\n"] {
        // Given
        let raw = format!("{}{separator}{}", paragraph(), paragraph());

        // When
        let chunks = collect(&raw, 100, 0);

        // Then
        assert_eq!(
            chunks[0].text,
            format!("{}{separator}", paragraph()),
            "separator {separator:?}"
        );
        for chunk in &chunks {
            assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
        }
    }
}

#[test]
fn never_splits_inside_a_crlf_pair() {
    // Given
    let raw = "abcdefghi\r\n".repeat(40);

    for max_chars in 12..=60 {
        // When
        let chunks = collect(&raw, max_chars, 0);

        // Then
        for chunk in &chunks {
            assert!(!chunk.text.starts_with('\n'), "max_chars {max_chars}");
            assert!(!chunk.text.ends_with('\r'), "max_chars {max_chars}");
            assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
        }
    }
}
