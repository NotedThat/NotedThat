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
    let Some((metadata, body_start)) = concept(path, raw) else {
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

fn concept(path: &str, raw: &str) -> Option<(ConceptMetadata, usize)> {
    let concept_id = path.strip_suffix(".md")?;
    if matches!(path.rsplit('/').next(), Some("index.md" | "log.md")) {
        return None;
    }
    let mut lines = raw.split_inclusive('\n');
    let opening = lines.next()?;
    if opening.trim_end_matches(['\r', '\n']) != "---" {
        return None;
    }
    let mut closing_start = opening.len();
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let value: Value = serde_yaml_ng::from_str(&raw[opening.len()..closing_start]).ok()?;
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
