//! OKF metadata extraction without rewriting source bytes.

use crate::chunker::{Chunk, chunk};
use notedthat_core::search::ConceptMetadata;
use serde_yaml_ng::Value;

/// Searchable body chunks and optional OKF concept metadata.
pub struct Document {
    /// Metadata is present only for concept documents with a non-empty string type.
    pub metadata: Option<ConceptMetadata>,
    /// Chunk offsets always address the original document, including its frontmatter.
    pub chunks: Vec<Chunk>,
}

/// Recognize OKF concepts, falling back to ordinary Markdown for other documents.
pub fn parse(path: &str, raw: &str) -> Document {
    let Some((metadata, body_start)) = concept_prefix(path, raw) else {
        return Document {
            metadata: None,
            chunks: chunk(raw),
        };
    };
    let mut chunks = chunk(&raw[body_start..]);
    for chunk in &mut chunks {
        chunk.byte_start += body_start;
        chunk.byte_end += body_start;
    }
    Document {
        metadata: Some(metadata),
        chunks,
    }
}

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
}
