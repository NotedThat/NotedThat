//! Integration tests for `QdrantProvisioner` against a real Qdrant instance.
//!
//! These tests are marked `#[ignore]` because they require Docker. Run with:
//! ```sh
//! cargo test -p notedthat-indexer --test provisioner_integration -- --ignored
//! ```
#![allow(missing_docs)]

mod support;
use support::{raw_client, start_qdrant};

use notedthat_core::KbSlug;
use notedthat_indexer::{QdrantClient, QdrantConfig, QdrantProvisioner};

fn make_config(url: &str) -> QdrantConfig {
    QdrantConfig {
        url: url.to_string(),
        api_key: None,
        ..Default::default()
    }
}

/// The payload indexes `ensure_collection` is expected to create.
const EXPECTED_INDEXES: [&str; 7] = [
    "object_key",
    "etag",
    "mime",
    "mtime",
    "heading_path",
    "tags",
    "okf.type",
];

/// Poll until every expected payload index appears, or fail at the deadline.
///
/// `create_field_index` is not called with `wait`, because blocking startup on
/// background index building would cost minutes on a large collection.
async fn await_indexes(
    raw: &qdrant_client::Qdrant,
    collection: &str,
) -> std::collections::HashMap<String, qdrant_client::qdrant::PayloadSchemaInfo> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let schema = raw
            .collection_info(collection)
            .await
            .expect("collection_info")
            .result
            .expect("collection info should have a result")
            .payload_schema;
        if EXPECTED_INDEXES.iter().all(|f| schema.contains_key(*f)) {
            return schema;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "payload indexes did not appear within 30s; have {:?}",
            schema.keys().collect::<Vec<_>>()
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn ensure_collection_creates_with_correct_schema() {
    let (_container, url) = start_qdrant().await;
    let client = QdrantClient::new(&make_config(&url)).expect("client build");
    let provisioner = QdrantProvisioner::new(client);
    let kb = KbSlug::try_new("test-kb").expect("valid slug");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection should succeed");

    let raw = raw_client(&url);
    let exists = raw
        .collection_exists("kb_test-kb_v1")
        .await
        .expect("collection_exists check");
    assert!(exists, "collection should exist after provisioning");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn ensure_collection_is_idempotent() {
    let (_container, url) = start_qdrant().await;
    let client = QdrantClient::new(&make_config(&url)).expect("client build");
    let provisioner = QdrantProvisioner::new(client);
    let kb = KbSlug::try_new("idempotent-kb").expect("valid slug");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("first call should succeed");
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("second call should succeed (idempotent)");

    let raw = raw_client(&url);
    let exists = raw
        .collection_exists("kb_idempotent-kb_v1")
        .await
        .expect("collection_exists check");
    assert!(
        exists,
        "collection should still exist after idempotent calls"
    );
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn upsert_and_tombstone_by_filter() {
    use qdrant_client::qdrant::{
        Condition, DeletePointsBuilder, Filter, PointStruct, UpsertPointsBuilder, Value,
    };
    use std::collections::HashMap;

    let (_container, url) = start_qdrant().await;
    let client = QdrantClient::new(&make_config(&url)).expect("client build");
    let provisioner = QdrantProvisioner::new(client);
    let kb = KbSlug::try_new("upsert-kb").expect("valid slug");
    let collection = "kb_upsert-kb_v1";

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection");

    let raw = raw_client(&url);

    let mut payload = HashMap::<String, Value>::new();
    payload.insert("object_key".to_string(), "test.md".to_string().into());
    payload.insert("chunk_index".to_string(), 0_i64.into());
    payload.insert("byte_start".to_string(), 0_i64.into());
    payload.insert("byte_end".to_string(), 10_i64.into());
    payload.insert("etag".to_string(), "\"abc123\"".to_string().into());
    payload.insert("mtime".to_string(), 1_700_000_000_i64.into());
    payload.insert("heading_path".to_string(), Vec::<String>::new().into());

    let vectors = HashMap::from([("dense".to_string(), vec![0.1_f32, 0.2, 0.3, 0.4])]);
    let point = PointStruct::new(1_u64, vectors, payload);

    raw.upsert_points(UpsertPointsBuilder::new(collection, vec![point]))
        .await
        .expect("upsert should succeed");

    let filter = Filter::must([Condition::matches("object_key", "test.md".to_string())]);
    raw.delete_points(DeletePointsBuilder::new(collection).points(filter))
        .await
        .expect("delete_points by filter should succeed");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn payload_indexes_created() {
    let (_container, url) = start_qdrant().await;
    let client = QdrantClient::new(&make_config(&url)).expect("client build");
    let provisioner = QdrantProvisioner::new(client);
    let kb = KbSlug::try_new("index-kb").expect("valid slug");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection");

    let raw = raw_client(&url);
    let schema = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let info = raw
                .collection_info("kb_index-kb_v1")
                .await
                .expect("collection_info");
            let schema = info
                .result
                .expect("collection info should have a result")
                .payload_schema;
            if schema.len() >= 5 || std::time::Instant::now() >= deadline {
                break schema;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    };

    assert!(
        schema.contains_key("object_key"),
        "object_key index missing"
    );
    assert!(schema.contains_key("etag"), "etag index missing");
    assert!(schema.contains_key("mime"), "mime index missing");
    assert!(schema.contains_key("mtime"), "mtime index missing");
    assert!(
        schema.contains_key("heading_path"),
        "heading_path index missing"
    );
    assert!(!schema.contains_key("tags"), "tags should NOT be indexed");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn ensure_collection_backfills_indexes_on_an_existing_collection() {
    // The upgrade path. `ensure_collection` used to return early when the
    // collection existed, so a release that added a payload index never applied
    // it to a running deployment. Without the fix this test fails on the second
    // call finding no indexes.
    let (_container, url) = start_qdrant().await;
    let provisioner = QdrantProvisioner::new(QdrantClient::new(&make_config(&url)).unwrap());
    let kb = KbSlug::try_new("backfill-kb").expect("valid slug");

    // Create the collection with no payload indexes at all, standing in for a
    // collection provisioned by an older release.
    let raw = raw_client(&url);
    {
        use qdrant_client::qdrant::{
            CreateCollectionBuilder, Distance, Modifier, SparseVectorParamsBuilder,
            SparseVectorsConfigBuilder, VectorParamsBuilder, VectorsConfigBuilder,
        };
        let mut vectors = VectorsConfigBuilder::default();
        vectors.add_named_vector_params("dense", VectorParamsBuilder::new(4, Distance::Cosine));
        let mut sparse = SparseVectorsConfigBuilder::default();
        sparse.add_named_vector_params(
            "sparse_bm25",
            SparseVectorParamsBuilder::default().modifier(Modifier::Idf),
        );
        raw.create_collection(
            CreateCollectionBuilder::new("kb_backfill-kb_v1")
                .vectors_config(vectors)
                .sparse_vectors_config(sparse),
        )
        .await
        .expect("pre-create collection");
    }

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection over an existing collection");
    // Idempotent: a second call must also succeed.
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection is idempotent");

    let schema = await_indexes(&raw, "kb_backfill-kb_v1").await;
    for field in EXPECTED_INDEXES {
        assert!(
            schema.contains_key(field),
            "payload index {field:?} was not backfilled; have {:?}",
            schema.keys().collect::<Vec<_>>()
        );
    }
}
