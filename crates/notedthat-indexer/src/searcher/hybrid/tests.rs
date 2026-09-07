use super::*;
use notedthat_core::search::SearchError;
use qdrant_client::qdrant::{ScoredPoint, Value, value::Kind};
use std::collections::HashMap;

#[test]
fn hybrid_searcher_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HybridSearcher>();
}

#[test]
fn collection_for_format() {
    use notedthat_core::KbSlug;
    let slug = KbSlug::try_new("notes").unwrap();
    assert_eq!(HybridSearcher::collection_for(&slug), "kb_notes_v1");
}

#[test]
fn search_error_from_qdrant_classifies_not_found_as_unknown_kb() {
    let err = search_error_from_qdrant(
        "kb_my-notes_v1",
        qdrant_client::QdrantError::ConversionError("collection not found".into()),
    );

    assert!(matches!(
        err,
        SearchError::UnknownKb { slug } if slug == "my-notes"
    ));
}

#[test]
fn search_error_from_qdrant_classifies_other_errors_as_backend_unavailable() {
    let err = search_error_from_qdrant(
        "kb_notes_v1",
        qdrant_client::QdrantError::ConversionError("transport closed".into()),
    );

    assert!(matches!(err, SearchError::BackendUnavailable { .. }));
}

#[test]
fn search_error_from_embedder_classifies_backend_unavailable() {
    let err = search_error_from_embedder(crate::embedder::EmbedderError::Transport(
        "connection refused".into(),
    ));

    assert!(matches!(err, SearchError::BackendUnavailable { .. }));
}

#[test]
fn point_to_hit_extracts_payload_fields() {
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "docs/a.md".to_string().into());
    payload.insert("byte_start".to_string(), 5_i64.into());
    payload.insert("byte_end".to_string(), 42_i64.into());
    payload.insert("heading_path".to_string(), vec!["A", "B"].into());
    payload.insert("text".to_string(), "hello world".to_string().into());

    let hit = point_to_hit(scored_point(payload, 0.5)).expect("valid hit");

    assert_eq!(hit.object_key.as_str(), "docs/a.md");
    assert_eq!(hit.byte_start, 5);
    assert_eq!(hit.byte_end, 42);
    assert_eq!(hit.heading_path, vec!["A", "B"]);
    assert!((hit.score - 0.5).abs() < f32::EPSILON);
    assert_eq!(hit.preview, "hello world");
    assert!(hit.okf.is_none());
}

#[test]
fn point_to_hit_decodes_okf_metadata() {
    let mut payload = HashMap::new();
    payload.insert("object_key".into(), "revenue.md".to_string().into());
    let metadata = serde_json::json!({
        "concept_id": "revenue", "type": "business-glossary", "title": "Revenue",
        "description": "Total income", "resource": "warehouse://revenue", "tags": ["finance"]
    });
    payload.insert("okf".into(), metadata.clone().into());
    let hit = point_to_hit(scored_point(payload, 0.5)).unwrap();
    assert_eq!(serde_json::to_value(hit.okf.unwrap()).unwrap(), metadata);
}

#[test]
fn point_to_hit_rejects_malformed_present_okf_metadata() {
    for metadata in [
        serde_json::json!({"concept_id": "revenue"}),
        serde_json::json!(null),
        serde_json::json!("invalid"),
    ] {
        let mut payload = HashMap::new();
        payload.insert("object_key".into(), "revenue.md".to_string().into());
        payload.insert("okf".into(), metadata.into());
        assert!(matches!(
            point_to_hit(scored_point(payload, 0.5)),
            Err(SearchError::Internal { .. })
        ));
    }
}

#[test]
fn point_to_hit_missing_text_yields_empty_preview() {
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "docs/a.md".to_string().into());
    payload.insert("byte_start".to_string(), 0_i64.into());
    payload.insert("byte_end".to_string(), 100_i64.into());

    let hit = point_to_hit(scored_point(payload, 0.5)).expect("valid hit");

    assert!(hit.preview.is_empty());
    assert_eq!(hit.object_key.as_str(), "docs/a.md");
    assert!((hit.score - 0.5).abs() < f32::EPSILON);
}

#[test]
fn point_to_hit_missing_object_key_returns_error() {
    let err = point_to_hit(scored_point(HashMap::new(), 0.5)).expect_err("missing object key");

    assert!(matches!(err, SearchError::Internal { .. }));
}

#[test]
fn point_to_hit_invalid_object_key_returns_error() {
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "/docs/a.md".to_string().into());

    let err = point_to_hit(scored_point(payload, 0.5)).expect_err("invalid object key");

    assert!(matches!(err, SearchError::Internal { .. }));
}

#[test]
fn point_to_hit_missing_heading_path_yields_empty_vec() {
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "docs/b.md".to_string().into());

    let hit = point_to_hit(scored_point(payload, 0.1)).expect("valid hit");

    assert!(hit.heading_path.is_empty());
}

#[test]
fn point_to_hit_negative_offsets_default_to_zero() {
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "docs/b.md".to_string().into());
    payload.insert("byte_start".to_string(), (-1_i64).into());
    payload.insert("byte_end".to_string(), (-2_i64).into());

    let hit = point_to_hit(scored_point(payload, 0.1)).expect("valid hit");

    assert_eq!(hit.byte_start, 0);
    assert_eq!(hit.byte_end, 0);
}

#[test]
fn preview_truncated_to_500_chars() {
    let long_text = "日本語".repeat(300);
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "docs/c.md".to_string().into());
    payload.insert("text".to_string(), long_text.into());

    let hit = point_to_hit(scored_point(payload, 0.2)).expect("valid hit");

    assert_eq!(hit.preview.chars().count(), 500);
}

#[test]
fn point_to_hit_ignores_non_string_heading_values() {
    let mut payload = HashMap::new();
    payload.insert("object_key".to_string(), "docs/d.md".to_string().into());
    payload.insert(
        "heading_path".to_string(),
        Value {
            kind: Some(Kind::ListValue(qdrant_client::qdrant::ListValue {
                values: vec!["A".to_string().into(), 1_i64.into(), "B".to_string().into()],
            })),
        },
    );

    let hit = point_to_hit(scored_point(payload, 0.3)).expect("valid hit");

    assert_eq!(hit.heading_path, vec!["A", "B"]);
}

fn scored_point(payload: HashMap<String, Value>, score: f32) -> ScoredPoint {
    ScoredPoint {
        id: None,
        payload,
        score,
        version: 0,
        vectors: None,
        shard_key: None,
        order_value: None,
    }
}
