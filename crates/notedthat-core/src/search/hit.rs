//! `SearchHit` — one hit in a search result set.

use serde::{Deserialize, Serialize};
use super::ObjectKey;

/// One hit in a search result set.
///
/// `byte_start` and `byte_end` are the byte offsets in the original object
/// that delimit this chunk. Use them with a `Range: bytes=<byte_start>-<byte_end-1>`
/// header against `GET /v1/knowledgebases/{kb_slug}/{path}` to fetch the exact bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The S3 object key (relative path within the knowledge base).
    pub object_key: ObjectKey,
    /// Byte offset of the start of this chunk in the original object.
    pub byte_start: u64,
    /// Byte offset of the end (exclusive) of this chunk in the original object.
    pub byte_end: u64,
    /// Heading hierarchy at the location of this chunk (may be empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<String>,
    /// RRF fusion rank score. Higher is better. Not a probability.
    pub score: f32,
    /// Preview text — a UTF-8-safe truncation of the chunk to at most 500 characters.
    pub preview: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hit() -> SearchHit {
        SearchHit {
            object_key: ObjectKey::try_new("docs/rfc/7231.md").unwrap(),
            byte_start: 1024,
            byte_end: 2048,
            heading_path: vec!["Section 1".into(), "Subsection 1.2".into()],
            score: 0.0163_f32,
            preview: "RFC 7231 defines HTTP semantics.".into(),
        }
    }

    #[test]
    fn serde_round_trip() {
        let hit = make_hit();
        let json = serde_json::to_string(&hit).unwrap();
        let back: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(back.object_key, hit.object_key);
        assert_eq!(back.byte_start, hit.byte_start);
        assert_eq!(back.byte_end, hit.byte_end);
        assert_eq!(back.heading_path, hit.heading_path);
        assert!((back.score - hit.score).abs() < 1e-6);
        assert_eq!(back.preview, hit.preview);
    }

    #[test]
    fn empty_heading_path_omitted_in_json() {
        let hit = SearchHit {
            object_key: ObjectKey::try_new("notes/a.md").unwrap(),
            byte_start: 0,
            byte_end: 100,
            heading_path: vec![],
            score: 0.5,
            preview: "preview text".into(),
        };
        let json = serde_json::to_string(&hit).unwrap();
        // heading_path is skip_serializing_if = "Vec::is_empty", so it should not appear
        assert!(!json.contains("heading_path"));
    }

    #[test]
    fn missing_heading_path_deserializes_as_empty() {
        let json = r#"{"object_key":"notes/a.md","byte_start":0,"byte_end":100,"score":0.5,"preview":"hello"}"#;
        let hit: SearchHit = serde_json::from_str(json).unwrap();
        assert!(hit.heading_path.is_empty());
    }

    #[test]
    fn emoji_in_preview_survives_round_trip() {
        let hit = SearchHit {
            object_key: ObjectKey::try_new("notes/b.md").unwrap(),
            byte_start: 0,
            byte_end: 50,
            heading_path: vec![],
            score: 0.1,
            preview: "Hello 🚀 World".into(),
        };
        let json = serde_json::to_string(&hit).unwrap();
        let back: SearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(back.preview, "Hello 🚀 World");
    }

    #[test]
    fn score_preserved() {
        let hit = make_hit();
        let json = serde_json::to_string(&hit).unwrap();
        let back: SearchHit = serde_json::from_str(&json).unwrap();
        assert!((back.score - 0.0163_f32).abs() < 1e-6);
    }

    #[test]
    fn send_sync_clone_debug() {
        fn assert<T: Send + Sync + Clone + std::fmt::Debug>() {}
        assert::<SearchHit>();
    }
}
