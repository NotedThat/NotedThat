//! Adversarial input tests for the streaming chunker indexing runs.
use std::io::Cursor;

use notedthat_indexer::chunker::stream_chunks;

#[test]
fn no_panic_on_adversarial_inputs() {
    let repeated_heading = "# ".repeat(100);
    let repeated_words = "word ".repeat(2000);
    let repeated_h = "# H\n".repeat(50);
    let inputs = vec![
        "",
        " ",
        "\n",
        "\t",
        "a",
        "# ",
        "#",
        "##",
        "# A",
        "# A\n",
        "# A\n## B",
        "## B",
        "### C",
        &repeated_heading,
        &repeated_words,
        "---\nfoo: bar\n---",
        "+++\nfoo = 'bar'\n+++",
        "{}",
        "# 日本語\n本文",
        "# A\n\n## B\n\n### C\n\n#### D",
        "\0",
        "a\nb\nc",
        "# A\r\n\r\nb\r\nc\r\n",
        "e\u{301}\u{1F44D}\u{1F3FD} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        &repeated_h,
    ];
    for max_chars in [1, 7, 3_000] {
        for input in &inputs {
            let chunks = stream_chunks(Cursor::new(input.as_bytes()), max_chars, 0)
                .expect("valid chunk iterator")
                .collect::<std::io::Result<Vec<_>>>()
                .expect("valid UTF-8 input");
            let shown = &input[..input.len().min(50)];
            for c in &chunks {
                assert_eq!(
                    &input[c.byte_start..c.byte_end],
                    c.text.as_str(),
                    "round-trip failed for max_chars={max_chars} input={shown:?}"
                );
                assert!(c.byte_start < c.byte_end);
                assert!(
                    c.text.chars().count() <= max_chars,
                    "over bound for max_chars={max_chars} input={shown:?}"
                );
            }
            // Whitespace-only chunks are dropped (D72), so the chunks cover the
            // input in order except for gaps that are only whitespace.
            let mut covered = 0;
            for c in &chunks {
                assert!(
                    input[covered..c.byte_start].trim().is_empty(),
                    "non-whitespace skipped for max_chars={max_chars} input={shown:?}"
                );
                covered = c.byte_end;
            }
            assert!(
                input[covered..].trim().is_empty(),
                "non-whitespace tail skipped for max_chars={max_chars} input={shown:?}"
            );
        }
    }
}
