//! Tests for the in-memory [`InMemoryVectorStore`] itself.
//!
//! Suites that used to run against a Qdrant container now assert against this
//! store, so its retrieval and filtering semantics are load-bearing: if the
//! fusion or the filter evaluation here is wrong, those suites pass while
//! meaning nothing. These pin the behaviour they rely on.
#![allow(missing_docs)]

use std::collections::HashMap;

use notedthat_core::KbSlug;
use notedthat_core::search::SearchFilter;
use notedthat_indexer::testing::InMemoryVectorStore;
use notedthat_indexer::vector_store::{HybridQuery, PointSelector, VectorStore, VectorStoreError};
use qdrant_client::qdrant::{PointStruct, Value, Vector};

fn kb() -> KbSlug {
    KbSlug::try_new("notes").expect("valid slug")
}

/// Build a point carrying a dense vector, BM25 text and the given payload.
fn point(id: u64, dense: Vec<f32>, text: &str, payload: Vec<(&str, Value)>) -> PointStruct {
    let mut fields = HashMap::<String, Value>::new();
    fields.insert("object_key".to_string(), format!("doc-{id}.md").into());
    fields.insert("chunk_index".to_string(), 0_i64.into());
    fields.insert("text".to_string(), text.to_string().into());
    for (key, value) in payload {
        fields.insert(key.to_string(), value);
    }

    let vectors = HashMap::from([
        ("dense".to_string(), Vector::from(dense)),
        (
            "sparse_bm25".to_string(),
            Vector::from(qdrant_client::qdrant::Document::new(
                text.to_string(),
                "qdrant/bm25",
            )),
        ),
    ]);
    PointStruct::new(id, vectors, fields)
}

async fn seeded(points: Vec<PointStruct>) -> InMemoryVectorStore {
    let store = InMemoryVectorStore::new();
    store.create_collection(&kb(), 3).await.expect("create");
    store.upsert_points(&kb(), points).await.expect("upsert");
    store
}

fn query(text: &str, dense: Vec<f32>, filter: Option<SearchFilter>) -> HybridQuery {
    HybridQuery {
        text: text.to_string(),
        dense,
        filter,
        prefetch_limit: 20,
        limit: 10,
    }
}

/// Read a result's `object_key`, which the tests use as the point's identity.
fn keys(points: &[qdrant_client::qdrant::ScoredPoint]) -> Vec<String> {
    points
        .iter()
        .filter_map(|point| match &point.payload.get("object_key")?.kind {
            Some(qdrant_client::qdrant::value::Kind::StringValue(key)) => Some(key.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn dense_arm_ranks_by_cosine_similarity() {
    // Only the dense arm can discriminate: no candidate shares a query term.
    let store = seeded(vec![
        point(1, vec![1.0, 0.0, 0.0], "alpha", vec![]),
        point(2, vec![0.0, 1.0, 0.0], "beta", vec![]),
    ])
    .await;

    let results = store
        .hybrid_search(&kb(), query("zzz", vec![0.9, 0.1, 0.0], None))
        .await
        .expect("search");

    assert_eq!(
        keys(&results).first().map(String::as_str),
        Some("doc-1.md"),
        "the vector closest in cosine terms must rank first"
    );
}

#[tokio::test]
async fn sparse_arm_ranks_by_term_overlap() {
    // Dense vectors are identical, so only BM25 can order these.
    let store = seeded(vec![
        point(1, vec![1.0, 0.0, 0.0], "the quick brown fox", vec![]),
        point(2, vec![1.0, 0.0, 0.0], "unrelated prose entirely", vec![]),
    ])
    .await;

    let results = store
        .hybrid_search(&kb(), query("quick fox", vec![1.0, 0.0, 0.0], None))
        .await
        .expect("search");

    assert_eq!(
        keys(&results).first().map(String::as_str),
        Some("doc-1.md"),
        "the document sharing query terms must rank first"
    );
}

#[tokio::test]
async fn fusion_prefers_a_document_both_arms_rank() {
    // doc-1 is second-best on each arm; doc-2 and doc-3 each win one arm and are
    // absent from the other. Reciprocal Rank Fusion should still favour doc-1.
    let store = seeded(vec![
        point(1, vec![0.9, 0.1, 0.0], "quick brown", vec![]),
        point(2, vec![1.0, 0.0, 0.0], "nothing in common here", vec![]),
        point(3, vec![0.0, 0.0, 1.0], "quick brown fox jumps", vec![]),
    ])
    .await;

    let results = store
        .hybrid_search(&kb(), query("quick brown", vec![1.0, 0.05, 0.0], None))
        .await
        .expect("search");

    assert_eq!(
        keys(&results).first().map(String::as_str),
        Some("doc-1.md"),
        "a document ranked by both arms must beat one that wins a single arm"
    );
}

#[tokio::test]
async fn limit_truncates_results() {
    let store = seeded(vec![
        point(1, vec![1.0, 0.0, 0.0], "alpha", vec![]),
        point(2, vec![0.9, 0.1, 0.0], "alpha", vec![]),
        point(3, vec![0.8, 0.2, 0.0], "alpha", vec![]),
    ])
    .await;

    let mut request = query("alpha", vec![1.0, 0.0, 0.0], None);
    request.limit = 2;
    let results = store.hybrid_search(&kb(), request).await.expect("search");

    assert_eq!(results.len(), 2, "outer limit must bound the result set");
}

#[tokio::test]
async fn filters_are_applied_before_ranking() {
    let store = seeded(vec![
        point(
            1,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("mime", "text/markdown".to_string().into())],
        ),
        point(
            2,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("mime", "text/plain".to_string().into())],
        ),
    ])
    .await;

    let filter = SearchFilter {
        mime: Some("text/plain".to_string()),
        ..SearchFilter::default()
    };
    let results = store
        .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], Some(filter)))
        .await
        .expect("search");

    assert_eq!(keys(&results), vec!["doc-2.md".to_string()]);
}

#[tokio::test]
async fn object_key_prefix_is_evaluated_by_the_store() {
    // The Qdrant path leaves this one to a client-side post-filter; the
    // in-memory store applies it directly, so the caller's post-filter is a
    // no-op rather than a correction.
    let store = seeded(vec![
        point(
            1,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("object_key", "notes/a.md".to_string().into())],
        ),
        point(
            2,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("object_key", "archive/b.md".to_string().into())],
        ),
    ])
    .await;

    let filter = SearchFilter {
        object_key_prefix: Some("notes/".to_string()),
        ..SearchFilter::default()
    };
    let results = store
        .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], Some(filter)))
        .await
        .expect("search");

    assert_eq!(keys(&results), vec!["notes/a.md".to_string()]);
}

#[tokio::test]
async fn heading_path_prefix_matches_on_a_leading_run() {
    let store = seeded(vec![
        point(
            1,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![(
                "heading_path",
                vec!["Guide".to_string(), "Setup".to_string()].into(),
            )],
        ),
        point(
            2,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![(
                "heading_path",
                vec!["Reference".to_string(), "Setup".to_string()].into(),
            )],
        ),
    ])
    .await;

    let filter = SearchFilter {
        heading_path_prefix: vec!["Guide".to_string()],
        ..SearchFilter::default()
    };
    let results = store
        .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], Some(filter)))
        .await
        .expect("search");

    assert_eq!(
        keys(&results),
        vec!["doc-1.md".to_string()],
        "a prefix must match from the start of heading_path, not anywhere in it"
    );
}

#[tokio::test]
async fn mtime_bounds_are_inclusive() {
    let store = seeded(vec![
        point(
            1,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("mtime", 100_i64.into())],
        ),
        point(
            2,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("mtime", 200_i64.into())],
        ),
        point(
            3,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("mtime", 300_i64.into())],
        ),
    ])
    .await;

    let filter = SearchFilter {
        updated_after: Some(200),
        updated_before: Some(300),
        ..SearchFilter::default()
    };
    let mut results = keys(
        &store
            .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], Some(filter)))
            .await
            .expect("search"),
    );
    results.sort();

    assert_eq!(
        results,
        vec!["doc-2.md".to_string(), "doc-3.md".to_string()]
    );
}

#[tokio::test]
async fn tags_match_any_of_the_requested_values() {
    let store = seeded(vec![
        point(
            1,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("tags", vec!["rust".to_string()].into())],
        ),
        point(
            2,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("tags", vec!["python".to_string()].into())],
        ),
    ])
    .await;

    let filter = SearchFilter {
        tags: vec!["rust".to_string(), "go".to_string()],
        ..SearchFilter::default()
    };
    let results = store
        .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], Some(filter)))
        .await
        .expect("search");

    assert_eq!(keys(&results), vec!["doc-1.md".to_string()]);
}

#[tokio::test]
async fn upsert_replaces_a_point_with_the_same_id() {
    let store = seeded(vec![point(1, vec![1.0, 0.0, 0.0], "original", vec![])]).await;
    store
        .upsert_points(
            &kb(),
            vec![point(1, vec![0.0, 1.0, 0.0], "replaced", vec![])],
        )
        .await
        .expect("re-upsert");

    assert_eq!(store.point_count(&kb()).await, Some(1));
    let results = store
        .hybrid_search(&kb(), query("replaced", vec![0.0, 1.0, 0.0], None))
        .await
        .expect("search");
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn deleting_an_object_removes_every_chunk_of_it() {
    let mut first = point(1, vec![1.0, 0.0, 0.0], "alpha", vec![]);
    let mut second = point(2, vec![1.0, 0.0, 0.0], "alpha", vec![]);
    first
        .payload
        .insert("object_key".into(), "same.md".to_string().into());
    second
        .payload
        .insert("object_key".into(), "same.md".to_string().into());
    second.payload.insert("chunk_index".into(), 1_i64.into());
    let store = seeded(vec![first, second]).await;

    store
        .delete_points(
            &kb(),
            PointSelector::Object {
                object_key: "same.md".to_string(),
            },
        )
        .await
        .expect("delete");

    assert_eq!(store.point_count(&kb()).await, Some(0));
}

#[tokio::test]
async fn deleting_trailing_chunks_keeps_the_ones_still_in_range() {
    let mut points = Vec::new();
    for index in 0..4_i64 {
        let mut chunk = point(
            u64::try_from(index).expect("small index"),
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![],
        );
        chunk
            .payload
            .insert("object_key".into(), "same.md".to_string().into());
        chunk.payload.insert("chunk_index".into(), index.into());
        points.push(chunk);
    }
    let store = seeded(points).await;

    // A re-index that produced two chunks must drop chunks 2 and 3.
    store
        .delete_points(
            &kb(),
            PointSelector::ObjectChunksFrom {
                object_key: "same.md".to_string(),
                from_chunk_index: 2,
            },
        )
        .await
        .expect("cleanup");

    assert_eq!(store.point_count(&kb()).await, Some(2));
}

#[tokio::test]
async fn deleting_trailing_chunks_leaves_other_objects_alone() {
    let mut mine = point(1, vec![1.0, 0.0, 0.0], "alpha", vec![]);
    mine.payload
        .insert("object_key".into(), "mine.md".to_string().into());
    mine.payload.insert("chunk_index".into(), 5_i64.into());
    let mut theirs = point(2, vec![1.0, 0.0, 0.0], "alpha", vec![]);
    theirs
        .payload
        .insert("object_key".into(), "theirs.md".to_string().into());
    theirs.payload.insert("chunk_index".into(), 5_i64.into());
    let store = seeded(vec![mine, theirs]).await;

    store
        .delete_points(
            &kb(),
            PointSelector::ObjectChunksFrom {
                object_key: "mine.md".to_string(),
                from_chunk_index: 0,
            },
        )
        .await
        .expect("cleanup");

    assert_eq!(
        store
            .indexed_object_keys(&kb())
            .await
            .into_iter()
            .collect::<Vec<_>>(),
        vec!["theirs.md".to_string()]
    );
}

#[tokio::test]
async fn searching_a_missing_collection_reports_collection_not_found() {
    let store = InMemoryVectorStore::new();
    let error = store
        .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], None))
        .await
        .expect_err("a KB with no collection must not search");

    assert!(matches!(error, VectorStoreError::CollectionNotFound { .. }));
}

#[tokio::test]
async fn concept_type_resolves_through_the_nested_okf_payload() {
    // The indexer writes `okf` as a nested struct and the filter addresses
    // `okf.type`, exactly as Qdrant resolves a dotted payload path. Treating it
    // as a flat key matched nothing and silently returned an empty result set.
    use qdrant_client::qdrant::{Struct, value::Kind};

    let okf = |concept_type: &str| Value {
        kind: Some(Kind::StructValue(Struct {
            fields: HashMap::from([("type".to_string(), Value::from(concept_type.to_string()))]),
        })),
    };

    let store = seeded(vec![
        point(
            1,
            vec![1.0, 0.0, 0.0],
            "alpha",
            vec![("okf", okf("Metric"))],
        ),
        point(2, vec![1.0, 0.0, 0.0], "alpha", vec![("okf", okf("Rule"))]),
    ])
    .await;

    let filter = SearchFilter {
        concept_type: Some("Metric".to_string()),
        ..SearchFilter::default()
    };
    let results = store
        .hybrid_search(&kb(), query("alpha", vec![1.0, 0.0, 0.0], Some(filter)))
        .await
        .expect("search");

    assert_eq!(keys(&results), vec!["doc-1.md".to_string()]);
}
