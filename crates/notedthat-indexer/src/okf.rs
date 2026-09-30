//! OKF metadata extraction without rewriting source bytes.

use notedthat_core::search::ConceptMetadata;
use serde_yaml_ng::Value;

/// Recognizes an OKF concept prefix, returning its metadata and the byte offset
/// where the body begins; `None` means the document is ordinary Markdown.
pub(crate) fn concept_prefix(path: &str, prefix: &str) -> Option<(ConceptMetadata, usize)> {
    let concept_id = path.strip_suffix(".md")?;
    if matches!(path.rsplit('/').next(), Some("index.md" | "log.md")) {
        return None;
    }
    let mut lines = prefix.split_inclusive('\n');
    let opening = lines.next()?;
    if opening.trim_end_matches(['\r', '\n']) != "---" {
        return None;
    }
    let mut closing_start = opening.len();
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let value: Value =
                serde_yaml_ng::from_str(&prefix[opening.len()..closing_start]).ok()?;
            let concept_type = value.get("type")?.as_str()?;
            if concept_type.trim().is_empty() {
                return None;
            }
            let optional_string =
                |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
            let tags = value
                .get("tags")
                .and_then(Value::as_sequence)
                .map(|tags| {
                    tags.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            return Some((
                ConceptMetadata {
                    concept_id: concept_id.to_owned(),
                    concept_type: concept_type.to_owned(),
                    title: optional_string("title"),
                    description: optional_string("description"),
                    resource: optional_string("resource"),
                    tags,
                },
                closing_start + line.len(),
            ));
        }
        closing_start += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{Chunk, SOFT_CHAR_CAP, stream_chunks};
    use std::io::Cursor;

    /// Chunks the body after a recognized prefix the way the indexer does, with
    /// offsets into the whole document.
    fn body_chunks(raw: &str, body_start: usize) -> Vec<Chunk> {
        stream_chunks(
            Cursor::new(&raw.as_bytes()[body_start..]),
            SOFT_CHAR_CAP,
            body_start,
        )
        .expect("valid chunk iterator")
        .collect::<std::io::Result<Vec<_>>>()
        .expect("valid UTF-8 body")
    }

    #[test]
    fn concept_prefix_returns_metadata_and_absolute_body_start() {
        // Given: a complete OKF frontmatter prefix without the document body.
        let prefix = "---\r\ntype: Metric\r\ntitle: Revenue\r\ntags: [finance]\r\n---\r\n";

        // When: the prefix is parsed independently of the rest of the snapshot.
        let (metadata, body_start) =
            concept_prefix("metrics/revenue.md", prefix).expect("complete OKF prefix should parse");

        // Then: metadata and the byte offset into the full source are preserved.
        assert_eq!(metadata.concept_id, "metrics/revenue");
        assert_eq!(metadata.concept_type, "Metric");
        assert_eq!(metadata.title.as_deref(), Some("Revenue"));
        assert_eq!(metadata.tags, ["finance"]);
        assert_eq!(body_start, prefix.len());
    }

    #[test]
    fn concept_prefix_rejects_frontmatter_without_closing_delimiter() {
        // Given: the maximum retained prefix ends inside OKF frontmatter.
        let prefix = "---\ntype: Metric\ntitle: Revenue";

        // When: bounded metadata recognition examines that prefix.
        let parsed = concept_prefix("metrics/revenue.md", prefix);

        // Then: it falls back to ordinary Markdown instead of reading beyond the cap.
        assert!(parsed.is_none());
    }

    #[test]
    fn concept_metadata_and_body_offsets_when_frontmatter_contains_extensions() {
        // Given: CRLF frontmatter with Unicode values and unknown extension keys.
        let raw = "---\r\ntype: Custom Concept\r\ntitle: 日本語\r\ndescription: >-\r\n  A linked\r\n  concept.\r\nresource: /other.md\r\ntags: [finance, 日本語]\r\nverified: {by: 'human:editor', at: '2026-09-07T00:00:00Z'}\r\ncustom: {nested: [1, true]}\r\n---\r\n# 本文\r\nSee [missing](/missing.md).\r\n";

        // When: the prefix is recognized and the body chunked after it.
        let (metadata, body_start) =
            concept_prefix("metrics/revenue.md", raw).expect("OKF concept");
        let chunks = body_chunks(raw, body_start);

        // Then: known fields are read and the body chunk addresses the original bytes.
        assert_eq!(metadata.concept_id, "metrics/revenue");
        assert_eq!(metadata.concept_type, "Custom Concept");
        assert_eq!(metadata.title.as_deref(), Some("日本語"));
        assert_eq!(metadata.description.as_deref(), Some("A linked concept."));
        assert_eq!(metadata.resource.as_deref(), Some("/other.md"));
        assert_eq!(metadata.tags, ["finance", "日本語"]);
        assert_eq!(chunks.len(), 1);
        for chunk in chunks {
            assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
            assert_eq!(chunk.byte_start, raw.find("# 本文").expect("body"));
            assert_eq!(chunk.heading_path, ["本文"]);
        }
    }

    #[test]
    fn ordinary_markdown_is_not_a_concept() {
        for raw in [
            "# Plain\nText",
            "---\ntitle: Legacy\n---\n# Body",
            "---\ntype: [broken\n---\n# Body",
            "---\ntype: Metric\n# Unclosed",
            "---\ntype: 42\n---\n# Body",
            "---\ntype: '  '\n---\n# Body",
            "---\n- Metric\n---\n# Body",
        ] {
            assert!(concept_prefix("note.md", raw).is_none(), "{raw}");
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
            assert!(concept_prefix(path, raw).is_none(), "{path}");
        }
    }

    #[test]
    fn minimal_concept_is_accepted_when_body_is_empty() {
        let raw = "---\ntype: Metric\n---";
        let (metadata, body_start) = concept_prefix("metric.md", raw).expect("minimal concept");
        assert_eq!(metadata.concept_type, "Metric");
        assert!(metadata.title.is_none());
        assert!(metadata.tags.is_empty());
        assert!(body_chunks(raw, body_start).is_empty());
    }

    #[test]
    fn invalid_optional_fields_do_not_reject_a_concept() {
        let (metadata, _) = concept_prefix(
            "metric.md",
            "---\ntype: Metric\ntitle: [odd]\ntags: [valid, 42]\n---\nBody",
        )
        .expect("optional guidance is soft");
        assert!(metadata.title.is_none());
        assert_eq!(metadata.tags, ["valid"]);
    }

    #[test]
    fn split_body_offsets_remain_absolute_when_unicode_exceeds_chunk_cap() {
        let raw = format!(
            "---\ntype: Reference\ntitle: 知識\n---\n# 日本語\n{}",
            "本文 ".repeat(3000)
        );
        let (_, body_start) = concept_prefix("reference.md", &raw).expect("OKF concept");
        let chunks = body_chunks(&raw, body_start);
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert_eq!(&raw[chunk.byte_start..chunk.byte_end], chunk.text);
            assert_eq!(chunk.heading_path, ["日本語"]);
        }
    }
}
