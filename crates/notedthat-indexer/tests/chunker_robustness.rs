//! Adversarial input tests for the chunker.
use notedthat_indexer::chunk;

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
        &repeated_h,
    ];
    for input in inputs {
        let chunks = chunk(input);
        for c in &chunks {
            assert_eq!(
                &input[c.byte_start..c.byte_end],
                c.text.as_str(),
                "round-trip failed for input={:?}",
                &input[..input.len().min(50)]
            );
            assert!(c.byte_start < c.byte_end);
        }
    }
}
