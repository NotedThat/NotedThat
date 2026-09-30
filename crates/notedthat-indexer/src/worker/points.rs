use crate::chunker;
use crate::mime::essence;
use notedthat_core::{ObjectMeta, ObjectPath, search::ConceptMetadata};
use qdrant_client::qdrant::{Document, PointStruct, Value, Vector};
use std::collections::HashMap;

pub(super) fn build_points(
    chunks: &[(usize, chunker::Chunk)],
    embeddings: &[Vec<f32>],
    object_key: &ObjectPath,
    meta: &ObjectMeta,
    content_hash: &str,
    metadata: Option<&ConceptMetadata>,
) -> Result<Vec<PointStruct>, String> {
    if chunks.len() != embeddings.len() {
        return Err(format!(
            "embedding count mismatch: chunks={} embeddings={}",
            chunks.len(),
            embeddings.len()
        ));
    }

    Ok(chunks
        .iter()
        .zip(embeddings.iter())
        .map(|((chunk_index, chunk), embedding)| {
            let mut payload = HashMap::<String, Value>::new();
            payload.insert(
                "object_key".to_string(),
                object_key.as_str().to_string().into(),
            );
            payload.insert(
                "chunk_index".to_string(),
                i64::try_from(*chunk_index).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "byte_start".to_string(),
                i64::try_from(chunk.byte_start).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "byte_end".to_string(),
                i64::try_from(chunk.byte_end).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "etag".to_string(),
                meta.etag.as_deref().unwrap_or("").into(),
            );
            payload.insert(
                "mime".to_string(),
                essence(meta.content_type.as_deref().unwrap_or("")).into(),
            );
            payload.insert("mtime".to_string(), meta.last_modified.unwrap_or(0).into());
            payload.insert(
                "heading_path".to_string(),
                chunk.heading_path.clone().into(),
            );
            payload.insert(
                "tags".to_string(),
                metadata
                    .map(|value| value.tags.clone())
                    .unwrap_or_default()
                    .into(),
            );
            if let Some(metadata) = metadata {
                payload.insert("okf".to_string(), serde_json::json!(metadata).into());
            }
            payload.insert("content_hash".to_string(), content_hash.to_string().into());
            payload.insert("text".to_string(), chunk.text.clone().into());

            let vectors = HashMap::from([
                ("dense".to_string(), Vector::from(embedding.clone())),
                (
                    "sparse_bm25".to_string(),
                    Vector::from(Document::new(chunk.text.clone(), "qdrant/bm25")),
                ),
            ]);
            PointStruct::new(point_id(object_key, *chunk_index), vectors, payload)
        })
        .collect())
}

pub(super) fn point_id(object_key: &ObjectPath, chunk_index: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let id = format!("{}/{}", object_key.as_str(), chunk_index);
    id.as_bytes().iter().fold(FNV_OFFSET, |hash, byte| {
        let hash = hash ^ u64::from(*byte);
        hash.wrapping_mul(FNV_PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrant_client::qdrant::value::Kind;

    #[test]
    fn mime_payload_is_the_media_type_essence() {
        let chunk = chunker::Chunk {
            text: "hello".to_string(),
            byte_start: 0,
            byte_end: 5,
            heading_path: Vec::new(),
        };
        let meta = ObjectMeta {
            key: "hello.md".to_string(),
            size: 5,
            last_modified: None,
            content_type: Some("Text/Markdown; charset=utf-8".to_string()),
            etag: None,
        };
        let key = ObjectPath::try_from("hello.md").expect("valid path");

        let points =
            build_points(&[(0, chunk)], &[vec![0.0]], &key, &meta, "hash", None).expect("points");

        let mime = points[0].payload.get("mime").and_then(|v| v.kind.clone());
        assert_eq!(mime, Some(Kind::StringValue("text/markdown".to_string())));
    }
}
