//! Conformance: [`QdrantClient`] and [`InMemoryVectorStore`] must agree.
//!
//! # Why this exists
//!
//! Every suite that used to run indexing and search against a Qdrant container
//! now runs against [`InMemoryVectorStore`]. That is a large win in wall-clock
//! time, but it moves all of `qdrant.rs` — the payload-key mapping in
//! `selector_filter`, the `SearchFilter` translation, the named-vector and IDF
//! configuration in `create_collection` — out of every test in the workspace.
//! Nothing else executes that code against a real Qdrant, so a mistranslation
//! there is invisible: the E2E suites stay green and production silently fails
//! to delete chunks, or scores nothing on the BM25 arm.
//!
//! This suite is the counterweight. It drives one scenario set through both
//! implementations of [`VectorStore`] and asserts the observations match, so the
//! substitute cannot drift away from the backend it stands in for. Run it after
//! any change to `src/qdrant.rs` or `src/testing.rs`.
//!
//! # What "agree" means here
//!
//! Observations are **sets of `object_key`s**, never scores or orderings. The
//! two implementations fuse differently by construction — Qdrant runs RRF
//! server-side over its own BM25 and HNSW arms, the substitute reimplements RRF
//! over a brute-force cosine scan and a textbook BM25 — so identical ranking is
//! neither achievable nor the property that matters. What must match is which
//! points a filter admits and which points a selector deletes, because that is
//! what the E2E suites are implicitly trusting.
//!
//! One divergence is deliberate and asserted as such rather than hidden:
//! `object_key_prefix` is a client-side post-filter (`searcher::filter`
//! documents why — qdrant-client 1.15 has no keyword prefix matcher), so
//! `HybridSearcher` applies it to the returned hits. Qdrant therefore ignores
//! it, while the substitute evaluates it. See `object_key_prefix_diverges`.
#![allow(missing_docs)]

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use notedthat_core::KbSlug;
use notedthat_core::search::SearchFilter;
use notedthat_indexer::testing::InMemoryVectorStore;
use notedthat_indexer::vector_store::{
    HybridQuery, PayloadFieldKind, PointSelector, VectorStore, VectorStoreError,
};
use notedthat_indexer::{QdrantClient, QdrantConfig};
use qdrant_client::qdrant::{Document, PointStruct, Value, Vector};
use testcontainers::{
    ContainerAsync, GenericImage,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

// ─── Container harness ──────────────────────────────────────────────────────

const QDRANT_IMAGE: &str = "qdrant/qdrant";
const QDRANT_TAG: &str = "v1.15.4";
const GRPC_PORT: u16 = 6334;
/// The line Qdrant prints once its gRPC listener is bound.
const GRPC_READY_LOG: &str = "Qdrant gRPC listening on 6334";
/// How long to wait for gRPC to answer after the container reports ready.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Start Qdrant and return it alongside a client that is ready to use.
///
/// Waiting on the log line is necessary but not sufficient: a bound port is not
/// a serving port, so `health_check` is polled afterwards. Skipping either step
/// produces the classic flake where collection calls succeed and the first
/// `upsert_points` dies as `Cancelled: Timeout expired`, which reads like a
/// Qdrant fault rather than a harness one.
///
/// The client is built through [`QdrantClient::new`] on purpose. A raw
/// `Qdrant::from_url(..).build()` inherits qdrant-client's 5-second per-RPC
/// default, which a `wait(true)` upsert can exceed on a loaded machine.
async fn start_qdrant() -> (ContainerAsync<GenericImage>, QdrantClient) {
    let container = GenericImage::new(QDRANT_IMAGE, QDRANT_TAG)
        .with_exposed_port(GRPC_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout(GRPC_READY_LOG))
        .start()
        .await
        .expect("failed to start Qdrant testcontainer — is Docker running?");

    let port = container
        .get_host_port_ipv4(GRPC_PORT)
        .await
        .expect("failed to get Qdrant gRPC port");
    let url = format!("http://127.0.0.1:{port}");

    await_grpc_ready(&url).await;

    let client = QdrantClient::new(&QdrantConfig {
        url,
        api_key: None,
        ..Default::default()
    })
    .expect("qdrant client build");

    (container, client)
}

async fn await_grpc_ready(url: &str) {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last_error;
    loop {
        match qdrant_client::Qdrant::from_url(url)
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => match client.health_check().await {
                Ok(_) => return,
                Err(err) => last_error = err.to_string(),
            },
            Err(err) => last_error = err.to_string(),
        }
        assert!(
            Instant::now() < deadline,
            "Qdrant gRPC at {url} did not become ready within {READY_TIMEOUT:?}; \
             last error: {last_error}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ─── Fixture ────────────────────────────────────────────────────────────────

const DENSE_DIM: u64 = 3;

fn slug(name: &str) -> KbSlug {
    KbSlug::try_new(name).expect("test KB slug is valid")
}

/// A point shaped like the ones `worker::points::build_points` writes.
///
/// Only the payload keys the filters and selectors read are set; the rest of
/// production's payload plays no part in translation.
struct Doc {
    object_key: &'static str,
    chunk_index: i64,
    dense: [f32; 3],
    text: &'static str,
    mime: &'static str,
    mtime: i64,
    heading_path: &'static [&'static str],
    tags: &'static [&'static str],
    okf_type: Option<&'static str>,
}

impl Doc {
    fn point(&self, id: u64) -> PointStruct {
        let mut payload = HashMap::<String, Value>::new();
        payload.insert("object_key".to_string(), self.object_key.to_string().into());
        payload.insert("chunk_index".to_string(), self.chunk_index.into());
        payload.insert("text".to_string(), self.text.to_string().into());
        payload.insert("mime".to_string(), self.mime.to_string().into());
        payload.insert("mtime".to_string(), self.mtime.into());
        payload.insert(
            "heading_path".to_string(),
            self.heading_path
                .iter()
                .map(|segment| (*segment).to_string())
                .collect::<Vec<_>>()
                .into(),
        );
        payload.insert(
            "tags".to_string(),
            self.tags
                .iter()
                .map(|tag| (*tag).to_string())
                .collect::<Vec<_>>()
                .into(),
        );
        if let Some(okf_type) = self.okf_type {
            payload.insert(
                "okf".to_string(),
                serde_json::json!({ "type": okf_type }).into(),
            );
        }

        let vectors = HashMap::from([
            ("dense".to_string(), Vector::from(self.dense.to_vec())),
            (
                "sparse_bm25".to_string(),
                Vector::from(Document::new(self.text.to_string(), "qdrant/bm25")),
            ),
        ]);
        PointStruct::new(id, vectors, payload)
    }
}

/// The corpus every filter scenario runs against.
///
/// Each field a filter can address varies across at least two documents, so a
/// translation that drops a condition admits too much and a translation that
/// mangles a payload key admits nothing — both show up as a set mismatch.
const CORPUS: &[Doc] = &[
    Doc {
        object_key: "alpha.md",
        chunk_index: 0,
        dense: [1.0, 0.0, 0.0],
        text: "alpha gamma shared",
        mime: "text/markdown",
        mtime: 1_000,
        heading_path: &["Guide", "Setup"],
        tags: &["red", "shared"],
        okf_type: Some("concept"),
    },
    Doc {
        object_key: "alpha.md",
        chunk_index: 1,
        dense: [0.9, 0.1, 0.0],
        text: "alpha delta shared",
        mime: "text/markdown",
        mtime: 2_000,
        heading_path: &["Guide", "Usage"],
        tags: &["blue", "shared"],
        okf_type: Some("concept"),
    },
    Doc {
        object_key: "alpha.md",
        chunk_index: 2,
        dense: [0.8, 0.2, 0.0],
        text: "alpha epsilon shared",
        mime: "text/markdown",
        mtime: 3_000,
        heading_path: &["Reference"],
        tags: &["red"],
        okf_type: None,
    },
    Doc {
        object_key: "beta.txt",
        chunk_index: 0,
        dense: [0.0, 1.0, 0.0],
        text: "beta zeta shared",
        mime: "text/plain",
        mtime: 4_000,
        heading_path: &["Guide", "Setup"],
        tags: &["blue"],
        okf_type: Some("relation"),
    },
    Doc {
        object_key: "gamma/nested.md",
        chunk_index: 0,
        dense: [0.0, 0.0, 1.0],
        text: "gamma eta shared",
        mime: "text/markdown",
        mtime: 5_000,
        heading_path: &["Reference"],
        tags: &["green"],
        okf_type: None,
    },
];

/// Provision `kb` exactly as `QdrantProvisioner` does, then load [`CORPUS`].
async fn seed(store: &dyn VectorStore, kb: &KbSlug) {
    store
        .create_collection(kb, DENSE_DIM)
        .await
        .expect("create_collection");
    for (field, kind) in [
        ("object_key", PayloadFieldKind::Keyword),
        ("etag", PayloadFieldKind::Keyword),
        ("mime", PayloadFieldKind::Keyword),
        ("mtime", PayloadFieldKind::Integer),
        ("heading_path", PayloadFieldKind::Keyword),
        ("tags", PayloadFieldKind::Keyword),
        ("okf.type", PayloadFieldKind::Keyword),
    ] {
        store
            .create_payload_index(kb, field, kind)
            .await
            .expect("create_payload_index");
    }

    let points: Vec<PointStruct> = CORPUS
        .iter()
        .enumerate()
        .map(|(index, doc)| doc.point(index as u64 + 1))
        .collect();
    store
        .upsert_points(kb, points)
        .await
        .expect("upsert_points");
}

/// Search with `filter` and return the matching `object_key`/`chunk_index`
/// pairs as a set.
///
/// `limit` is deliberately larger than the corpus: this asks "which points does
/// the backend admit?", not "how does it rank them?".
async fn matching(
    store: &dyn VectorStore,
    kb: &KbSlug,
    filter: Option<SearchFilter>,
) -> BTreeSet<String> {
    let hits = store
        .hybrid_search(
            kb,
            HybridQuery {
                text: "shared".to_string(),
                dense: vec![1.0, 1.0, 1.0],
                filter,
                prefetch_limit: 100,
                limit: 100,
            },
        )
        .await
        .expect("hybrid_search");

    hits.iter()
        .map(|hit| {
            let key = string_field(hit.payload.get("object_key")).expect("hit carries object_key");
            let chunk = int_field(hit.payload.get("chunk_index")).expect("hit carries chunk_index");
            format!("{key}#{chunk}")
        })
        .collect()
}

fn string_field(value: Option<&Value>) -> Option<String> {
    match &value?.kind {
        Some(qdrant_client::qdrant::value::Kind::StringValue(text)) => Some(text.clone()),
        _ => None,
    }
}

fn int_field(value: Option<&Value>) -> Option<i64> {
    match &value?.kind {
        Some(qdrant_client::qdrant::value::Kind::IntegerValue(number)) => Some(*number),
        _ => None,
    }
}

/// Collapse an error to its variant, so the two backends can be compared
/// without depending on Qdrant's wording.
fn error_kind(error: &VectorStoreError) -> &'static str {
    match error {
        VectorStoreError::CollectionNotFound { .. } => "CollectionNotFound",
        VectorStoreError::Backend { .. } => "Backend",
    }
}

// ─── Scenarios ──────────────────────────────────────────────────────────────

/// One named observation produced identically by both backends.
type Observations = Vec<(&'static str, String)>;

/// Every filter field `translate_filter` can express, run against one corpus.
///
/// Table-driven so a newly supported filter field is one row, and so the
/// scenario names line up positionally between the two backends.
async fn observe_filters(store: &dyn VectorStore, kb: &KbSlug) -> Observations {
    seed(store, kb).await;

    let cases: Vec<(&'static str, Option<SearchFilter>)> = vec![
        // The baseline. If this stops returning the whole corpus, every
        // comparison below starts passing for the wrong reason.
        ("no_filter", None),
        (
            "mime",
            Some(SearchFilter {
                mime: Some("text/plain".to_string()),
                ..SearchFilter::default()
            }),
        ),
        // Nested payload: the condition addresses `okf.type`, not `type`.
        (
            "concept_type",
            Some(SearchFilter {
                concept_type: Some("concept".to_string()),
                ..SearchFilter::default()
            }),
        ),
        // Array prefix, expressed as per-index equality on heading_path[i].
        (
            "heading_path_prefix",
            Some(SearchFilter {
                heading_path_prefix: vec!["Guide".to_string(), "Setup".to_string()],
                ..SearchFilter::default()
            }),
        ),
        (
            "heading_path_prefix_partial",
            Some(SearchFilter {
                heading_path_prefix: vec!["Guide".to_string()],
                ..SearchFilter::default()
            }),
        ),
        // Range bounds are inclusive on both ends; 3_000 and 2_000 are corpus
        // values, so an accidental `gt`/`lt` drops a document.
        (
            "updated_after",
            Some(SearchFilter {
                updated_after: Some(3_000),
                ..SearchFilter::default()
            }),
        ),
        (
            "updated_before",
            Some(SearchFilter {
                updated_before: Some(2_000),
                ..SearchFilter::default()
            }),
        ),
        (
            "updated_between",
            Some(SearchFilter {
                updated_after: Some(2_000),
                updated_before: Some(4_000),
                ..SearchFilter::default()
            }),
        ),
        // MatchAny: "red" OR "green", not both.
        (
            "tags_any",
            Some(SearchFilter {
                tags: vec!["red".to_string(), "green".to_string()],
                ..SearchFilter::default()
            }),
        ),
        // Several conditions AND-composed into one Filter::must.
        (
            "combined",
            Some(SearchFilter {
                mime: Some("text/markdown".to_string()),
                tags: vec!["red".to_string()],
                updated_after: Some(2_000),
                ..SearchFilter::default()
            }),
        ),
    ];

    let mut out: Observations = Vec::with_capacity(cases.len());
    for (name, filter) in cases {
        let admitted = matching(store, kb, filter).await;
        out.push((name, admitted.into_iter().collect::<Vec<_>>().join(",")));
    }
    out
}

/// `PointSelector` translation — the payload keys and the `gte` boundary.
async fn observe_deletes(store: &dyn VectorStore, kb: &KbSlug) -> Observations {
    seed(store, kb).await;

    let mut out: Observations = Vec::new();

    // Trailing-chunk cleanup after a re-index that produced fewer chunks.
    // The boundary is inclusive: chunk 1 must go, chunk 0 must stay.
    store
        .delete_points(
            kb,
            PointSelector::ObjectChunksFrom {
                object_key: "alpha.md".to_string(),
                from_chunk_index: 1,
            },
        )
        .await
        .expect("delete ObjectChunksFrom");
    out.push((
        "after_object_chunks_from",
        matching(store, kb, None)
            .await
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
    ));

    // Tombstone: every chunk of one object, and nothing belonging to another.
    store
        .delete_points(
            kb,
            PointSelector::Object {
                object_key: "alpha.md".to_string(),
            },
        )
        .await
        .expect("delete Object");
    out.push((
        "after_object",
        matching(store, kb, None)
            .await
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
    ));

    out
}

/// Collection lifecycle and the missing-collection error.
async fn observe_lifecycle(store: &dyn VectorStore, kb: &KbSlug) -> Observations {
    let mut out: Observations = Vec::new();

    out.push((
        "exists_before_create",
        format!("{:?}", store.collection_exists(kb).await.expect("exists")),
    ));

    let missing = slug("neverprovisioned");
    let error = store
        .hybrid_search(
            &missing,
            HybridQuery {
                text: "shared".to_string(),
                dense: vec![1.0, 1.0, 1.0],
                filter: None,
                prefetch_limit: 10,
                limit: 10,
            },
        )
        .await
        .expect_err("searching a missing collection must fail");
    out.push(("search_missing_collection", error_kind(&error).to_string()));

    store
        .create_collection(kb, DENSE_DIM)
        .await
        .expect("create_collection");
    out.push((
        "exists_after_create",
        format!("{:?}", store.collection_exists(kb).await.expect("exists")),
    ));

    // Provisioning is re-run on every startup so a new index backfills onto an
    // existing collection; creating one twice must therefore be idempotent.
    out.push((
        "payload_index_is_idempotent",
        format!(
            "{:?}",
            store
                .create_payload_index(kb, "mime", PayloadFieldKind::Keyword)
                .await
                .and(
                    store
                        .create_payload_index(kb, "mime", PayloadFieldKind::Keyword)
                        .await
                )
                .is_ok()
        ),
    ));

    out
}

fn assert_agree(qdrant: &Observations, memory: &Observations) {
    assert_eq!(
        qdrant.len(),
        memory.len(),
        "both backends must produce the same observations"
    );
    for ((name, from_qdrant), (memory_name, from_memory)) in qdrant.iter().zip(memory) {
        assert_eq!(name, memory_name, "observation order must match");
        assert_eq!(
            from_qdrant, from_memory,
            "'{name}': QdrantClient and InMemoryVectorStore disagree. \
             Qdrant admitted [{from_qdrant}], the substitute admitted [{from_memory}]. \
             One of src/qdrant.rs or src/testing.rs is wrong; the E2E suites \
             cannot see this."
        );
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a Qdrant testcontainer"]
async fn filters_admit_the_same_points_in_both_backends() {
    let (_container, qdrant) = start_qdrant().await;
    let kb = slug("filters");

    let from_qdrant = observe_filters(&qdrant, &kb).await;
    let from_memory = observe_filters(&InMemoryVectorStore::new(), &kb).await;

    // Guard the premise: a filter that admits everything would make every
    // comparison below pass for the wrong reason.
    let no_filter = &from_qdrant[0];
    assert_eq!(no_filter.0, "no_filter");
    assert_eq!(
        no_filter.1.split(',').count(),
        CORPUS.len(),
        "the unfiltered query must return the whole corpus, otherwise the \
         filtered comparisons prove nothing"
    );

    assert_agree(&from_qdrant, &from_memory);
}

#[tokio::test]
#[ignore = "requires a Qdrant testcontainer"]
async fn selectors_delete_the_same_points_in_both_backends() {
    let (_container, qdrant) = start_qdrant().await;
    let kb = slug("deletes");

    let from_qdrant = observe_deletes(&qdrant, &kb).await;
    let from_memory = observe_deletes(&InMemoryVectorStore::new(), &kb).await;

    assert_agree(&from_qdrant, &from_memory);
}

#[tokio::test]
#[ignore = "requires a Qdrant testcontainer"]
async fn collection_lifecycle_matches_in_both_backends() {
    let (_container, qdrant) = start_qdrant().await;
    let kb = slug("lifecycle");

    let from_qdrant = observe_lifecycle(&qdrant, &kb).await;
    let from_memory = observe_lifecycle(&InMemoryVectorStore::new(), &kb).await;

    assert_agree(&from_qdrant, &from_memory);
}

#[tokio::test]
#[ignore = "requires a Qdrant testcontainer"]
async fn both_backends_write_a_usable_sparse_arm() {
    // The sparse vector is easy to lose: write only "dense" and every call still
    // succeeds, search still returns hits, and the BM25 arm is silently gone.
    // Nothing but a query that ONLY the sparse arm can answer catches it — here
    // the dense vector is orthogonal to the query, so a dense-only backend
    // cannot rank the term-matching document first.
    let (_container, qdrant) = start_qdrant().await;
    let kb = slug("sparsearm");

    for (label, store) in [
        ("QdrantClient", &qdrant as &dyn VectorStore),
        ("InMemoryVectorStore", &InMemoryVectorStore::new()),
    ] {
        seed(store, &kb).await;

        let hits = store
            .hybrid_search(
                &kb,
                HybridQuery {
                    text: "zeta".to_string(),
                    dense: vec![0.0, 0.0, 0.0],
                    filter: None,
                    prefetch_limit: 100,
                    limit: 1,
                },
            )
            .await
            .expect("hybrid_search");

        let top = hits
            .first()
            .and_then(|hit| string_field(hit.payload.get("object_key")))
            .unwrap_or_else(|| panic!("{label} returned no hit for a term-only query"));
        assert_eq!(
            top, "beta.txt",
            "{label}: 'zeta' appears only in beta.txt, so the BM25 arm must \
             surface it; a missing or unmodified sparse vector shows up here"
        );
    }
}

#[tokio::test]
#[ignore = "requires a Qdrant testcontainer"]
async fn object_key_prefix_diverges_because_it_is_a_post_filter() {
    // Pin the ONE place the two backends are meant to differ, so it stays a
    // documented decision rather than becoming an undiagnosed bug. qdrant-client
    // 1.15 has no keyword prefix matcher, so `translate_filter` routes
    // `object_key_prefix` to a client-side PostFilter that `HybridSearcher`
    // applies to the hits. Qdrant never sees it; the substitute evaluates the
    // whole filter, which is a harmless superset because the caller re-applies
    // it either way.
    let (_container, qdrant) = start_qdrant().await;
    let kb = slug("prefixfilter");

    let filter = Some(SearchFilter {
        object_key_prefix: Some("gamma/".to_string()),
        ..SearchFilter::default()
    });

    seed(&qdrant, &kb).await;
    let from_qdrant = matching(&qdrant, &kb, filter.clone()).await;

    let memory = InMemoryVectorStore::new();
    seed(&memory, &kb).await;
    let from_memory = matching(&memory, &kb, filter).await;

    assert_eq!(
        from_qdrant.len(),
        CORPUS.len(),
        "Qdrant must ignore object_key_prefix — if it starts honouring it, \
         HybridSearcher's over-fetch multiplier is no longer needed"
    );
    assert_eq!(
        from_memory,
        BTreeSet::from(["gamma/nested.md#0".to_string()]),
        "the substitute evaluates object_key_prefix itself"
    );
}
