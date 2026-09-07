//! Integration tests for the OKF v0.2 search filters and payload indexes (D48).
//!
//! These exist because the interesting parts cannot be proved by a unit test on a
//! `Filter` struct. `exclude_stale` is a nested OR folded into the outer AND, and
//! a nested filter is exactly the kind of thing that can be silently
//! mis-serialised on the wire; and the provisioner's index backfill is a claim
//! about a live collection.
//!
//! Each test requires a running Qdrant container (Docker) and is marked
//! `#[ignore]`. Run with:
//! `cargo test -p notedthat-indexer --test okf_search_integration -- --ignored`

#![allow(missing_docs)]

mod support;
use support::{raw_client, start_qdrant};

use notedthat_core::{
    KbSlug,
    okf::{OkfStatus, OkfTrust},
    search::{SearchFilter, SearchRequest},
};
use notedthat_indexer::{
    Embedder, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, QdrantClient, QdrantConfig,
    QdrantProvisioner, Searcher, searcher::HybridSearcher,
};
use std::{sync::Arc, time::Duration};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

static INTEGRATION_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Comfortably in the past, so `exclude_stale` must drop it.
const PAST: i64 = 1_000_000_000;
/// Comfortably in the future, so `exclude_stale` must keep it.
const FUTURE: i64 = 4_000_000_000;

fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let v: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0 } else { 0.0 })
                .collect();
            serde_json::json!({ "index": i, "embedding": v, "object": "embedding" })
        })
        .collect();
    serde_json::json!({ "object": "list", "data": data })
}

fn make_embedder(server_uri: &str, dim: usize) -> Arc<dyn Embedder> {
    Arc::new(
        OpenAiCompatibleEmbedder::new(OpenAiCompatibleConfig {
            endpoint_url: server_uri.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dim,
            max_input_tokens: 8192,
            timeout: Duration::from_secs(10),
            max_retries: 3,
        })
        .expect("embedder construction failed"),
    )
}

fn make_qdrant(url: &str) -> (Arc<QdrantClient>, QdrantProvisioner) {
    let cfg = QdrantConfig {
        url: url.to_string(),
        api_key: None,
        ..Default::default()
    };
    let client = Arc::new(QdrantClient::new(&cfg).unwrap());
    let provisioner = QdrantProvisioner::new(QdrantClient::new(&cfg).unwrap());
    (client, provisioner)
}

fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").unwrap()
}

fn coll(kb: &KbSlug) -> String {
    format!("kb_{}_v1", kb.as_str())
}

/// How one seeded point is shaped. `None` for the OKF fields means the point is
/// a plain non-OKF document.
struct Seed<'a> {
    id: u64,
    object_key: &'a str,
    text: &'a str,
    okf_type: Option<&'a str>,
    trust: OkfTrust,
    status: OkfStatus,
    stale_after: Option<i64>,
    runtime: Option<&'a str>,
    chunk_kind: &'a str,
}

impl<'a> Seed<'a> {
    fn okf(id: u64, object_key: &'a str, text: &'a str, okf_type: &'a str) -> Self {
        Self {
            id,
            object_key,
            text,
            okf_type: Some(okf_type),
            trust: OkfTrust::Unverified,
            status: OkfStatus::Stable,
            stale_after: None,
            runtime: None,
            chunk_kind: "body",
        }
    }

    fn plain(id: u64, object_key: &'a str, text: &'a str) -> Self {
        Self {
            id,
            object_key,
            text,
            okf_type: None,
            trust: OkfTrust::Unverified,
            status: OkfStatus::Stable,
            stale_after: None,
            runtime: None,
            chunk_kind: "body",
        }
    }
}

async fn seed(qdrant: &qdrant_client::Qdrant, collection: &str, s: &Seed<'_>) {
    use qdrant_client::qdrant::{Document, PointStruct, UpsertPointsBuilder, Value, Vector};
    use std::collections::HashMap;

    let mut payload = HashMap::<String, Value>::new();
    payload.insert("object_key".to_string(), s.object_key.to_string().into());
    payload.insert("chunk_index".to_string(), 0_i64.into());
    payload.insert("byte_start".to_string(), 0_i64.into());
    payload.insert(
        "byte_end".to_string(),
        i64::try_from(s.text.len()).unwrap_or(i64::MAX).into(),
    );
    payload.insert("etag".to_string(), "deadbeef".to_string().into());
    payload.insert("content_hash".to_string(), "deadbeef".to_string().into());
    payload.insert("mtime".to_string(), 1_000_i64.into());
    payload.insert("mime".to_string(), "text/markdown".to_string().into());
    payload.insert("heading_path".to_string(), Vec::<String>::new().into());
    payload.insert("tags".to_string(), Vec::<String>::new().into());
    payload.insert("text".to_string(), s.text.to_string().into());
    payload.insert("chunk_kind".to_string(), s.chunk_kind.to_string().into());

    if let Some(okf_type) = s.okf_type {
        payload.insert("okf_type".to_string(), okf_type.to_string().into());
        payload.insert(
            "okf_status".to_string(),
            s.status.as_str().to_string().into(),
        );
        payload.insert("okf_trust".to_string(), s.trust.as_str().to_string().into());
        if let Some(stale_after) = s.stale_after {
            payload.insert("okf_stale_after".to_string(), stale_after.into());
        }
        if let Some(runtime) = s.runtime {
            payload.insert("okf_runtime".to_string(), runtime.to_string().into());
        }
    }

    let vectors = HashMap::from([
        (
            "dense".to_string(),
            Vector::from(vec![1.0_f32, 0.0, 0.0, 0.0]),
        ),
        (
            "sparse_bm25".to_string(),
            Vector::from(Document::new(s.text.to_string(), "qdrant/bm25")),
        ),
    ]);

    qdrant
        .upsert_points(
            UpsertPointsBuilder::new(collection, vec![PointStruct::new(s.id, vectors, payload)])
                .wait(true),
        )
        .await
        .expect("seed upsert failed");
}

/// Seed a fixed corpus covering every OKF filter dimension.
async fn seed_corpus(raw: &qdrant_client::Qdrant, collection: &str) {
    // 1: plain non-OKF document.
    seed(raw, collection, &Seed::plain(1, "plain.md", "shared token")).await;

    // 2: OKF, unverified, no expiry.
    seed(
        raw,
        collection,
        &Seed::okf(2, "metric.md", "shared token", "Metric"),
    )
    .await;

    // 3: OKF, human-reviewed, expiry in the future.
    seed(
        raw,
        collection,
        &Seed {
            trust: OkfTrust::HumanReviewed,
            stale_after: Some(FUTURE),
            ..Seed::okf(3, "fresh.md", "shared token", "Metric")
        },
    )
    .await;

    // 4: OKF, machine-confirmed, expiry in the past — the stale one.
    seed(
        raw,
        collection,
        &Seed {
            trust: OkfTrust::MachineConfirmed,
            stale_after: Some(PAST),
            ..Seed::okf(4, "stale.md", "shared token", "Metric")
        },
    )
    .await;

    // 5: OKF, deprecated, a different type, an Attested Computation runtime.
    seed(
        raw,
        collection,
        &Seed {
            status: OkfStatus::Deprecated,
            runtime: Some("bigquery"),
            ..Seed::okf(5, "rev.md", "shared token", "Attested Computation")
        },
    )
    .await;

    // 6: an OKF metadata point rather than a body chunk.
    seed(
        raw,
        collection,
        &Seed {
            chunk_kind: "metadata",
            ..Seed::okf(6, "meta.md", "shared token", "Metric")
        },
    )
    .await;
}

/// Run one search and return the object keys it matched.
async fn keys_for(url: &str, filter: SearchFilter) -> Vec<String> {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let (qdrant_client, _) = make_qdrant(url);
    let searcher = HybridSearcher::new(qdrant_client, make_embedder(&mock_server.uri(), 4));
    let request = SearchRequest {
        query: "shared token".to_string(),
        filter: Some(filter),
        limit: Some(50),
    }
    .validate()
    .expect("request validates");

    let mut keys: Vec<String> = searcher
        .search(&kb(), request)
        .await
        .expect("search failed")
        .hits
        .into_iter()
        .map(|hit| hit.object_key.as_str().to_string())
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

async fn prepared() -> (impl std::any::Any, String) {
    let (container, url) = start_qdrant().await;
    let (_, provisioner) = make_qdrant(&url);
    provisioner
        .ensure_collection(&kb(), 4)
        .await
        .expect("ensure_collection failed");
    seed_corpus(&raw_client(&url), &coll(&kb())).await;
    (container, url)
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn no_filter_returns_everything_including_stale_and_unverified() {
    // The D48 default: return everything and annotate it. OKF §11 forbids
    // rejecting a concept for missing trust data.
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let keys = keys_for(&url, SearchFilter::default()).await;
    assert_eq!(keys.len(), 6, "expected every seeded point, got {keys:?}");
    assert!(keys.contains(&"stale.md".to_string()));
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn exclude_stale_keeps_documents_with_no_expiry() {
    // The `is_empty` leg of the nested OR. This is the one a mis-serialised
    // nested filter would silently break, by dropping every document that simply
    // states no `stale_after`.
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let keys = keys_for(
        &url,
        SearchFilter {
            exclude_stale: true,
            ..Default::default()
        },
    )
    .await;

    assert!(
        !keys.contains(&"stale.md".to_string()),
        "stale kept: {keys:?}"
    );
    assert!(
        keys.contains(&"fresh.md".to_string()),
        "future expiry dropped: {keys:?}"
    );
    assert!(
        keys.contains(&"metric.md".to_string()),
        "no-expiry OKF dropped: {keys:?}"
    );
    assert!(
        keys.contains(&"plain.md".to_string()),
        "non-OKF dropped: {keys:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn okf_type_filters_to_one_concept_kind() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let keys = keys_for(
        &url,
        SearchFilter {
            okf_type: Some("Attested Computation".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(keys, vec!["rev.md".to_string()]);
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn okf_min_trust_is_inclusive_of_higher_tiers() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;

    let confirmed = keys_for(
        &url,
        SearchFilter {
            okf_min_trust: Some(OkfTrust::MachineConfirmed),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        confirmed,
        vec!["fresh.md".to_string(), "stale.md".to_string()]
    );

    let human = keys_for(
        &url,
        SearchFilter {
            okf_min_trust: Some(OkfTrust::HumanReviewed),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(human, vec!["fresh.md".to_string()]);
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn okf_status_matches_documents_that_state_no_status() {
    // Absent `status` is indexed as "stable", which is what makes this a plain
    // equality rather than an OR-with-empty.
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let stable = keys_for(
        &url,
        SearchFilter {
            okf_status: Some(OkfStatus::Stable),
            ..Default::default()
        },
    )
    .await;
    assert!(stable.contains(&"metric.md".to_string()));
    assert!(!stable.contains(&"rev.md".to_string()));
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn okf_only_excludes_non_okf_documents() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let keys = keys_for(
        &url,
        SearchFilter {
            okf_only: true,
            ..Default::default()
        },
    )
    .await;
    assert!(!keys.contains(&"plain.md".to_string()), "{keys:?}");
    assert_eq!(keys.len(), 5);
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn chunk_kind_selects_metadata_points() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let keys = keys_for(
        &url,
        SearchFilter {
            chunk_kind: Some("metadata".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(keys, vec!["meta.md".to_string()]);
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn okf_runtime_finds_attested_computations() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;
    let keys = keys_for(
        &url,
        SearchFilter {
            okf_runtime: Some("bigquery".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(keys, vec!["rev.md".to_string()]);
}

#[tokio::test]
#[ignore = "requires Docker for the Qdrant testcontainer"]
async fn hits_carry_their_okf_annotation() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (_container, url) = prepared().await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;
    let (qdrant_client, _) = make_qdrant(&url);
    let searcher = HybridSearcher::new(qdrant_client, make_embedder(&mock_server.uri(), 4));
    let request = SearchRequest {
        query: "shared token".to_string(),
        filter: None,
        limit: Some(50),
    }
    .validate()
    .unwrap();

    let hits = searcher.search(&kb(), request).await.unwrap().hits;
    let find = |key: &str| {
        hits.iter()
            .find(|hit| hit.object_key.as_str() == key)
            .unwrap_or_else(|| panic!("{key} missing from {hits:?}"))
    };

    // A non-OKF document carries no annotation at all.
    assert!(find("plain.md").okf.is_none());

    let stale = find("stale.md").okf.as_ref().expect("annotated");
    assert_eq!(stale.concept_type, "Metric");
    assert_eq!(stale.trust, OkfTrust::MachineConfirmed);
    assert!(stale.stale, "past stale_after must read as stale");

    let fresh = find("fresh.md").okf.as_ref().expect("annotated");
    assert_eq!(fresh.trust, OkfTrust::HumanReviewed);
    assert!(!fresh.stale);

    // An absent `stale_after` is never stale.
    assert!(!find("metric.md").okf.as_ref().expect("annotated").stale);
}
