//! Integration tests for `QdrantProvisioner` against an in-memory vector store.
//!
//! The subject is the provisioner's own logic — create the collection only when
//! it is absent, and ensure every payload index on *every* call — so these run
//! against [`InMemoryVectorStore`] rather than a Qdrant container. The store
//! records collections, their dense width and their payload indexes, which is
//! all these assertions read.
//!
//! Run with: `cargo test -p notedthat-indexer --test provisioner_integration`
#![allow(missing_docs)]

use std::sync::Arc;

use notedthat_core::KbSlug;
use notedthat_indexer::testing::InMemoryVectorStore;
use notedthat_indexer::vector_store::{PointSelector, VectorStore};
use notedthat_indexer::{QdrantProvisioner, vector_store::PayloadFieldKind};

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

/// A provisioner and the store behind it, so assertions can inspect the store.
fn provisioner() -> (QdrantProvisioner, InMemoryVectorStore) {
    let store = InMemoryVectorStore::new();
    (QdrantProvisioner::new(Arc::new(store.clone())), store)
}

fn kb(slug: &str) -> KbSlug {
    KbSlug::try_new(slug).expect("valid slug")
}

#[tokio::test]
async fn ensure_collection_creates_with_correct_schema() {
    let (provisioner, store) = provisioner();
    let kb = kb("test-kb");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection should succeed");

    assert!(
        store.collection_exists(&kb).await.expect("exists check"),
        "collection should exist after provisioning"
    );
    assert_eq!(
        store.dense_dim(&kb).await,
        Some(4),
        "collection should carry the requested dense width"
    );
}

#[tokio::test]
async fn ensure_collection_is_idempotent() {
    let (provisioner, store) = provisioner();
    let kb = kb("idempotent-kb");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("first call should succeed");
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("second call should succeed (idempotent)");

    assert!(
        store.collection_exists(&kb).await.expect("exists check"),
        "collection should still exist after idempotent calls"
    );
    assert_eq!(
        store.point_count(&kb).await,
        Some(0),
        "provisioning must not invent points"
    );
}

#[tokio::test]
async fn upsert_and_tombstone_by_object_key() {
    use qdrant_client::qdrant::{PointStruct, Value};
    use std::collections::HashMap;

    let (provisioner, store) = provisioner();
    let kb = kb("upsert-kb");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection");

    let mut payload = HashMap::<String, Value>::new();
    payload.insert("object_key".to_string(), "test.md".to_string().into());
    payload.insert("chunk_index".to_string(), 0_i64.into());
    payload.insert("byte_start".to_string(), 0_i64.into());
    payload.insert("byte_end".to_string(), 10_i64.into());
    payload.insert("etag".to_string(), "\"abc123\"".to_string().into());
    payload.insert("mtime".to_string(), 1_700_000_000_i64.into());
    payload.insert("heading_path".to_string(), Vec::<String>::new().into());

    let vectors = HashMap::from([("dense".to_string(), vec![0.1_f32, 0.2, 0.3, 0.4])]);
    store
        .upsert_points(&kb, vec![PointStruct::new(1_u64, vectors, payload)])
        .await
        .expect("upsert should succeed");
    assert_eq!(store.point_count(&kb).await, Some(1));

    store
        .delete_points(
            &kb,
            PointSelector::Object {
                object_key: "test.md".to_string(),
            },
        )
        .await
        .expect("delete by object key should succeed");
    assert_eq!(
        store.point_count(&kb).await,
        Some(0),
        "tombstoning an object should remove all of its chunks"
    );
}

#[tokio::test]
async fn payload_indexes_created() {
    let (provisioner, store) = provisioner();
    let kb = kb("index-kb");

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection");

    let indexes = store.payload_indexes(&kb).await;
    for field in EXPECTED_INDEXES {
        assert!(indexes.contains(field), "payload index {field:?} missing");
    }
}

#[tokio::test]
async fn ensure_collection_backfills_indexes_on_an_existing_collection() {
    // The upgrade path. `ensure_collection` used to return early when the
    // collection existed, so a release that added a payload index never applied
    // it to a running deployment. Without the fix this test fails on the second
    // call finding no indexes.
    let (provisioner, store) = provisioner();
    let kb = kb("backfill-kb");

    // Stand in for a collection provisioned by an older release: it exists, but
    // carries none of the payload indexes the current release expects.
    store
        .create_collection(&kb, 4)
        .await
        .expect("pre-create collection");
    assert!(
        store.payload_indexes(&kb).await.is_empty(),
        "precondition: the pre-existing collection has no payload indexes"
    );

    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection over an existing collection");
    // Idempotent: a second call must also succeed.
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection is idempotent");

    let indexes = store.payload_indexes(&kb).await;
    for field in EXPECTED_INDEXES {
        assert!(
            indexes.contains(field),
            "payload index {field:?} was not backfilled; have {indexes:?}"
        );
    }
}

#[tokio::test]
async fn payload_index_on_a_missing_collection_is_an_error() {
    // The provisioner always creates the collection first; this pins the store
    // contract that ordering relies on.
    let store = InMemoryVectorStore::new();
    let result = store
        .create_payload_index(&kb("absent-kb"), "object_key", PayloadFieldKind::Keyword)
        .await;

    assert!(
        result.is_err(),
        "indexing a field on a collection that does not exist must fail"
    );
}
