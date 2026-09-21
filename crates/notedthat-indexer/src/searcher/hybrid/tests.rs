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
fn search_error_from_store_maps_collection_not_found_to_unknown_kb() {
    let err = search_error_from_store(
        "kb_my-notes_v1",
        VectorStoreError::CollectionNotFound {
            kb: "my-notes".into(),
        },
    );

    assert!(matches!(
        err,
        SearchError::UnknownKb { slug } if slug == "my-notes"
    ));
}

#[test]
fn search_error_from_store_classifies_not_found_backend_text_as_unknown_kb() {
    // A backend that reports a missing collection as a plain transport error
    // must still surface as UnknownKb rather than an outage; the collection
    // name is what the slug is recovered from.
    let err = search_error_from_store(
        "kb_my-notes_v1",
        VectorStoreError::Backend {
            message: "collection not found".into(),
        },
    );

    assert!(matches!(
        err,
        SearchError::UnknownKb { slug } if slug == "my-notes"
    ));
}

#[test]
fn search_error_from_store_classifies_other_errors_as_backend_unavailable() {
    let err = search_error_from_store(
        "kb_notes_v1",
        VectorStoreError::Backend {
            message: "transport closed".into(),
        },
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

// ---------------------------------------------------------------------------
// `search` itself: the fetch window (#68) and the order of hits (#128).
//
// `InMemoryVectorStore` cannot stand in for Qdrant here: it evaluates
// `object_key_prefix` natively and breaks ties by point id, so through it
// neither the starvation nor the instability is observable. This double is
// shaped like Qdrant in exactly the ways under test — it ignores the prefix,
// hands fusion no more than the two arms can hold, truncates to `limit`, and
// returns its points in whatever order the test scripted.
// ---------------------------------------------------------------------------

mod search {
    use super::*;
    use crate::searcher::{KeyPredicate, Searcher};
    use crate::testing::StubEmbedder;
    use crate::vector_store::{
        HybridQuery, IndexedObject, PayloadFieldKind, PointSelector, VectorStore, VectorStoreError,
    };
    use async_trait::async_trait;
    use notedthat_core::KbSlug;
    use notedthat_core::search::{SearchFilter, SearchRequest, SearchResponse, ValidatedRequest};
    use qdrant_client::qdrant::PointStruct;
    use std::sync::{Arc, Mutex};

    struct ScriptedStore {
        points: Vec<ScoredPoint>,
        queries: Mutex<Vec<HybridQuery>>,
    }

    impl ScriptedStore {
        fn new(points: Vec<ScoredPoint>) -> Arc<Self> {
            Arc::new(Self {
                points,
                queries: Mutex::new(Vec::new()),
            })
        }

        fn last_query(&self) -> HybridQuery {
            self.queries
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("the searcher queried the store")
        }
    }

    #[async_trait]
    impl VectorStore for ScriptedStore {
        async fn collection_exists(&self, _: &KbSlug) -> Result<bool, VectorStoreError> {
            unimplemented!()
        }
        async fn create_collection(&self, _: &KbSlug, _: u64) -> Result<(), VectorStoreError> {
            unimplemented!()
        }
        async fn create_payload_index(
            &self,
            _: &KbSlug,
            _: &str,
            _: PayloadFieldKind,
        ) -> Result<(), VectorStoreError> {
            unimplemented!()
        }
        async fn upsert_points(
            &self,
            _: &KbSlug,
            _: Vec<PointStruct>,
        ) -> Result<(), VectorStoreError> {
            unimplemented!()
        }
        async fn delete_points(
            &self,
            _: &KbSlug,
            _: PointSelector,
        ) -> Result<(), VectorStoreError> {
            unimplemented!()
        }
        async fn indexed_etag(
            &self,
            _: &KbSlug,
            _: &str,
        ) -> Result<Option<String>, VectorStoreError> {
            unimplemented!()
        }
        async fn indexed_objects(
            &self,
            _: &KbSlug,
            _: Option<&str>,
        ) -> Result<Vec<IndexedObject>, VectorStoreError> {
            unimplemented!()
        }

        async fn hybrid_search(
            &self,
            _: &KbSlug,
            query: HybridQuery,
        ) -> Result<Vec<ScoredPoint>, VectorStoreError> {
            // The fused set is the union of the two arms, so at most
            // `2 * prefetch_limit` candidates exist, and the backend cuts that
            // to `limit` in the order it holds them.
            let available = usize::try_from(query.prefetch_limit * 2).unwrap_or(usize::MAX);
            let limit = usize::try_from(query.limit).unwrap_or(usize::MAX);
            self.queries.lock().unwrap().push(query);
            Ok(self
                .points
                .iter()
                .take(available.min(limit))
                .cloned()
                .collect())
        }
    }

    fn keyed_point(key: &str, byte_start: u64, score: f32) -> ScoredPoint {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), Value::from(key));
        payload.insert(
            "byte_start".to_string(),
            Value::from(i64::try_from(byte_start).unwrap()),
        );
        payload.insert(
            "byte_end".to_string(),
            Value::from(i64::try_from(byte_start).unwrap() + 1),
        );
        payload.insert("text".to_string(), Value::from(format!("text of {key}")));
        scored_point(payload, score)
    }

    fn searcher(store: Arc<ScriptedStore>) -> HybridSearcher {
        HybridSearcher::new(store, Arc::new(StubEmbedder::new(4)))
    }

    fn request(limit: u32, prefix: Option<&str>) -> ValidatedRequest {
        SearchRequest {
            query: "anything".to_string(),
            filter: prefix.map(|prefix| SearchFilter {
                object_key_prefix: Some(prefix.to_string()),
                ..Default::default()
            }),
            limit: Some(limit),
        }
        .validate()
        .unwrap()
    }

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    fn keys(response: &SearchResponse) -> Vec<String> {
        response
            .hits
            .iter()
            .map(|hit| format!("{}#{}", hit.object_key.as_str(), hit.byte_start))
            .collect()
    }

    /// 45 private chunks the backend ranks first, then the 5 public ones the
    /// caller may see: exactly the shape that starved under the old window.
    fn starved_corpus() -> Vec<ScoredPoint> {
        (0..45)
            .map(|i| keyed_point(&format!("private/{i:02}.md"), 0, 0.5))
            .chain((0..5).map(|i| keyed_point(&format!("public/{i}.md"), 0, 0.4)))
            .collect()
    }

    #[test]
    fn fetch_window_derives_both_limits_from_one_computation() {
        // (limit, native, post) -> (prefetch, fused)
        let table = [
            ((10, false, false), (20, 40)),
            ((50, false, false), (50, 100)),
            ((10, true, false), (100, 200)),
            ((10, false, true), (100, 200)),
            ((5, false, true), (50, 100)),
            ((50, false, true), (250, 500)),
            ((50, true, true), (250, 500)),
            ((1, true, false), (100, 200)),
        ];
        for ((limit, native, post), (prefetch, fused)) in table {
            let window = fetch_window(limit, native, post);
            assert_eq!(
                (window.prefetch_limit, window.limit),
                (prefetch, fused),
                "limit={limit} native={native} post={post}"
            );
        }
        for limit in 1..=50 {
            for native in [false, true] {
                for post in [false, true] {
                    let window = fetch_window(limit, native, post);
                    assert_eq!(window.limit, window.prefetch_limit * 2);
                    assert!(window.prefetch_limit >= u64::from(limit));
                    assert!(window.limit <= 500);
                }
            }
        }
    }

    #[tokio::test]
    async fn object_key_prefix_is_served_from_the_over_fetched_window() {
        let store = ScriptedStore::new(starved_corpus());
        let response = searcher(store)
            .search(&kb(), request(5, Some("public/")), None)
            .await
            .unwrap();

        assert_eq!(
            keys(&response),
            [
                "public/0.md#0",
                "public/1.md#0",
                "public/2.md#0",
                "public/3.md#0",
                "public/4.md#0"
            ]
        );
    }

    #[tokio::test]
    async fn key_filter_is_served_from_the_over_fetched_window() {
        let store = ScriptedStore::new(starved_corpus());
        let public: KeyPredicate<'_> = &|key| key.starts_with("public/");
        let response = searcher(store)
            .search(&kb(), request(5, None), Some(public))
            .await
            .unwrap();

        assert_eq!(response.hits.len(), 5);
        assert!(
            response
                .hits
                .iter()
                .all(|hit| hit.object_key.as_str().starts_with("public/"))
        );
    }

    #[tokio::test]
    async fn key_filter_and_prefix_compose_as_an_intersection() {
        let store = ScriptedStore::new(starved_corpus());
        let not_three: KeyPredicate<'_> = &|key| key != "public/3.md";
        let response = searcher(store)
            .search(&kb(), request(5, Some("public/")), Some(not_three))
            .await
            .unwrap();

        assert_eq!(
            keys(&response),
            [
                "public/0.md#0",
                "public/1.md#0",
                "public/2.md#0",
                "public/4.md#0"
            ]
        );
    }

    #[tokio::test]
    async fn the_query_sent_carries_the_window() {
        let store = ScriptedStore::new(starved_corpus());
        let subject = searcher(store.clone());

        subject
            .search(&kb(), request(5, Some("public/")), None)
            .await
            .unwrap();
        let sent = store.last_query();
        let expected = fetch_window(5, false, true);
        assert_eq!(
            (sent.prefetch_limit, sent.limit),
            (expected.prefetch_limit, expected.limit)
        );

        subject.search(&kb(), request(5, None), None).await.unwrap();
        let sent = store.last_query();
        let expected = fetch_window(5, false, false);
        assert_eq!(
            (sent.prefetch_limit, sent.limit),
            (expected.prefetch_limit, expected.limit)
        );

        let allow_all: KeyPredicate<'_> = &|_| true;
        subject
            .search(&kb(), request(5, None), Some(allow_all))
            .await
            .unwrap();
        let sent = store.last_query();
        let expected = fetch_window(5, false, true);
        assert_eq!(
            (sent.prefetch_limit, sent.limit),
            (expected.prefetch_limit, expected.limit)
        );
    }

    fn tied_corpus() -> Vec<ScoredPoint> {
        vec![
            keyed_point("b.md", 0, 0.5),
            keyed_point("a.md", 100, 0.5),
            keyed_point("z.md", 0, 0.75),
            keyed_point("a.md", 0, 0.5),
            keyed_point("c.md", 0, 0.5),
        ]
    }

    #[tokio::test]
    async fn tied_scores_are_ordered_by_key_then_offset_whatever_the_backend_order() {
        let scripted = searcher(ScriptedStore::new(tied_corpus()))
            .search(&kb(), request(10, None), None)
            .await
            .unwrap();
        let reversed = searcher(ScriptedStore::new(
            tied_corpus().into_iter().rev().collect(),
        ))
        .search(&kb(), request(10, None), None)
        .await
        .unwrap();

        assert_eq!(
            keys(&scripted),
            ["z.md#0", "a.md#0", "a.md#100", "b.md#0", "c.md#0"]
        );
        assert_eq!(scripted, reversed);
    }

    #[tokio::test]
    async fn membership_at_the_limit_boundary_is_stable_under_ties() {
        // The measured case: four chunks tied for three slots.
        let corpus: Vec<ScoredPoint> = ["d.md", "b.md", "c.md", "a.md"]
            .into_iter()
            .map(|key| keyed_point(key, 0, 0.5))
            .collect();

        let scripted = searcher(ScriptedStore::new(corpus.clone()))
            .search(&kb(), request(3, None), None)
            .await
            .unwrap();
        let reversed = searcher(ScriptedStore::new(corpus.into_iter().rev().collect()))
            .search(&kb(), request(3, None), None)
            .await
            .unwrap();

        assert_eq!(keys(&scripted), ["a.md#0", "b.md#0", "c.md#0"]);
        assert_eq!(
            serde_json::to_vec(&scripted).unwrap(),
            serde_json::to_vec(&reversed).unwrap()
        );
    }

    #[tokio::test]
    async fn a_key_filter_rejecting_everything_yields_no_hits() {
        let nothing: KeyPredicate<'_> = &|_| false;
        let response = searcher(ScriptedStore::new(starved_corpus()))
            .search(&kb(), request(5, None), Some(nothing))
            .await
            .unwrap();

        assert!(response.hits.is_empty());
    }
}
