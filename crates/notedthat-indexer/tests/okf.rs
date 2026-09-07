//! OKF recognition and source-offset regression tests.

use notedthat_indexer::okf::parse;

#[test]
fn concept_metadata_and_body_offsets_when_frontmatter_contains_extensions() {
    let raw = "---\r\ntype: Custom Concept\r\ntitle: 日本語\r\ndescription: >-\r\n  A linked\r\n  concept.\r\nresource: /other.md\r\ntags: [finance, 日本語]\r\nverified: {by: 'human:editor', at: '2026-09-07T00:00:00Z'}\r\ncustom: {nested: [1, true]}\r\n---\r\n# 本文\r\nSee [missing](/missing.md).\r\n";
    let document = parse("metrics/revenue.md", raw);
    let metadata = document.metadata.expect("OKF concept");
    assert_eq!(metadata.concept_id, "metrics/revenue");
    assert_eq!(metadata.concept_type, "Custom Concept");
    assert_eq!(metadata.title.as_deref(), Some("日本語"));
    assert_eq!(metadata.description.as_deref(), Some("A linked concept."));
    assert_eq!(metadata.resource.as_deref(), Some("/other.md"));
    assert_eq!(metadata.tags, ["finance", "日本語"]);
    assert_eq!(document.chunks.len(), 1);
    for chunk in document.chunks {
        assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
        assert_eq!(chunk.byte_start, raw.find("# 本文").expect("body"));
        assert_eq!(chunk.heading_path, ["本文"]);
    }
}

#[test]
fn ordinary_markdown_is_preserved_when_not_an_okf_concept() {
    for raw in [
        "# Plain\nText",
        "---\ntitle: Legacy\n---\n# Body",
        "---\ntype: [broken\n---\n# Body",
        "---\ntype: Metric\n# Unclosed",
        "---\ntype: 42\n---\n# Body",
        "---\ntype: '  '\n---\n# Body",
        "---\n- Metric\n---\n# Body",
    ] {
        let document = parse("note.md", raw);
        assert!(document.metadata.is_none(), "{raw}");
        assert_eq!(document.chunks, notedthat_indexer::chunk(raw));
    }
}

#[test]
fn reserved_and_non_markdown_files_are_not_concepts() {
    let raw = "---\ntype: Metric\n---\n# Body";
    for path in [
        "index.md",
        "log.md",
        "nested/index.md",
        "nested/log.md",
        "note.txt",
    ] {
        let document = parse(path, raw);
        assert!(document.metadata.is_none(), "{path}");
        assert_eq!(document.chunks, notedthat_indexer::chunk(raw));
    }
}

#[test]
fn minimal_concept_is_accepted_when_body_is_empty() {
    let document = parse("metric.md", "---\ntype: Metric\n---");
    let metadata = document.metadata.expect("minimal concept");
    assert_eq!(metadata.concept_type, "Metric");
    assert!(metadata.title.is_none());
    assert!(metadata.tags.is_empty());
    assert!(document.chunks.is_empty());
}

#[test]
fn invalid_optional_fields_do_not_reject_a_concept() {
    let document = parse(
        "metric.md",
        "---\ntype: Metric\ntitle: [odd]\ntags: [valid, 42]\n---\nBody",
    );
    let metadata = document.metadata.expect("optional guidance is soft");
    assert!(metadata.title.is_none());
    assert_eq!(metadata.tags, ["valid"]);
}

#[test]
fn split_body_offsets_remain_absolute_when_unicode_exceeds_chunk_cap() {
    let raw = format!(
        "---\ntype: Reference\ntitle: 知識\n---\n# 日本語\n{}",
        "本文 ".repeat(3000)
    );
    let document = parse("reference.md", &raw);
    assert!(document.chunks.len() > 1);
    for chunk in document.chunks {
        assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
        assert_eq!(chunk.heading_path, ["日本語"]);
    }
}
