#![allow(missing_docs)]
//! Integration tests for the OKF v0.2 routes (D48).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use notedthat_api_http::router::build_router;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::{InMemoryStorage, NoopSearcher};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage};
use tower::util::ServiceExt;

const TOKEN: &str = "test-token-abc";
const KB: &str = "notes";

fn declared_kbs() -> BTreeMap<String, KbSlug> {
    let mut kbs = BTreeMap::new();
    kbs.insert(KB.to_string(), KbSlug::try_new(KB).unwrap());
    kbs
}

/// A router over an in-memory bundle seeded with `objects`.
async fn app_with(objects: &[(&str, &str)]) -> axum::Router {
    let storage = Arc::new(InMemoryStorage::default());
    let kb = KbSlug::try_new(KB).unwrap();
    storage.ensure_bucket(&kb).await.unwrap();
    for (key, body) in objects {
        storage
            .put_object(
                &kb,
                &ObjectPath::try_from_str(key).unwrap(),
                bytes::Bytes::from((*body).to_string()),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();
    }

    let (indexer_tx, rx) = tokio::sync::mpsc::channel(1024);
    // Keep the receiver alive so `commit` never sees a closed channel.
    Box::leak(Box::new(rx));
    let state = AppState {
        storage,
        declared_kbs: Arc::new(declared_kbs()),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    };
    build_router(state)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn post(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

const CONFORMANT: &str = "---\ntype: BigQuery Table\ntitle: Customers\n---\n# Schema\n\ncols\n";
const NO_TYPE: &str = "---\ntitle: Orders\n---\n# Body\n";
const NO_FRONTMATTER: &str = "# Just markdown\n";

// --- validate ---

#[tokio::test]
async fn test1_conformant_bundle_returns_conformant_true() {
    let app = app_with(&[("tables/customers.md", CONFORMANT)]).await;
    let (status, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["conformant"], true);
    assert_eq!(body["scanned"], 1);
    assert_eq!(body["counts"]["error"], 0);
}

#[tokio::test]
async fn test2_missing_type_is_a_conformance_error() {
    let app = app_with(&[("tables/orders.md", NO_TYPE)]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["conformant"], false);
    assert_eq!(body["findings"][0]["rule"], "type_missing");
    assert_eq!(body["findings"][0]["severity"], "error");
    assert_eq!(body["findings"][0]["path"], "tables/orders.md");
}

#[tokio::test]
async fn test3_missing_frontmatter_is_a_conformance_error() {
    let app = app_with(&[("a.md", NO_FRONTMATTER)]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["findings"][0]["rule"], "frontmatter_missing");
}

#[tokio::test]
async fn test4_broken_link_is_a_warning_not_an_error() {
    // OKF §11 requires consumers to tolerate broken links, so a validator that
    // failed a bundle on one would contradict the spec it validates.
    let doc = "---\ntype: Metric\n---\nsee [gone](./missing.md)\n";
    let app = app_with(&[("a.md", doc)]).await;
    let (_, body) = send(
        &app,
        post("/v1/okf/notes/validate", r#"{"check_links":true}"#),
    )
    .await;
    assert_eq!(body["conformant"], true);
    assert_eq!(body["counts"]["warning"], 1);
    assert_eq!(body["findings"][0]["rule"], "broken_link");
}

#[tokio::test]
async fn test5_link_checking_is_off_by_default() {
    let doc = "---\ntype: Metric\n---\nsee [gone](./missing.md)\n";
    let app = app_with(&[("a.md", doc)]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["counts"]["warning"], 0);
}

#[tokio::test]
async fn test6_an_absolute_url_link_is_never_reported_broken() {
    // It is also never fetched: the server must not dereference a user URL.
    let doc = "---\ntype: Metric\n---\nsee [x](https://example.com/nope.md)\n";
    let app = app_with(&[("a.md", doc)]).await;
    let (_, body) = send(
        &app,
        post("/v1/okf/notes/validate", r#"{"check_links":true}"#),
    )
    .await;
    assert_eq!(body["counts"]["warning"], 0);
}

#[tokio::test]
async fn test7_a_resolvable_link_is_not_reported_broken() {
    let doc = "---\ntype: Metric\n---\nsee [c](./customers.md)\n";
    let app = app_with(&[("tables/a.md", doc), ("tables/customers.md", CONFORMANT)]).await;
    let (_, body) = send(
        &app,
        post("/v1/okf/notes/validate", r#"{"check_links":true}"#),
    )
    .await;
    assert_eq!(body["counts"]["warning"], 0);
}

#[tokio::test]
async fn test8_unknown_type_values_are_tolerated() {
    let app = app_with(&[("a.md", "---\ntype: Wibble\n---\n")]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["conformant"], true);
}

#[tokio::test]
async fn test9_reserved_files_are_not_checked_as_concepts() {
    let app = app_with(&[("index.md", "# Tables\n\n* [A](./a.md)\n")]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["conformant"], true);
    assert_eq!(body["counts"]["error"], 0);
}

#[tokio::test]
async fn test10_root_okf_version_is_surfaced() {
    let app = app_with(&[("index.md", "---\nokf_version: \"0.2\"\n---\n# T\n")]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["okf_version"], "0.2");
}

#[tokio::test]
async fn test11_non_markdown_objects_are_skipped_without_fetching() {
    let app = app_with(&[("a.md", CONFORMANT), ("data.csv", "a,b\n")]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", "{}")).await;
    assert_eq!(body["scanned"], 1);
    assert_eq!(body["skipped_non_markdown"], 1);
}

#[tokio::test]
async fn test12_single_object_mode_checks_only_that_object() {
    let app = app_with(&[("a.md", CONFORMANT), ("b.md", NO_TYPE)]).await;
    let (_, body) = send(&app, post("/v1/okf/notes/validate", r#"{"path":"a.md"}"#)).await;
    assert_eq!(body["scanned"], 1);
    assert_eq!(body["conformant"], true);
}

#[tokio::test]
async fn test13_path_and_prefix_together_returns_400() {
    let app = app_with(&[]).await;
    let (status, body) = send(
        &app,
        post("/v1/okf/notes/validate", r#"{"path":"a.md","prefix":"x/"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn test14_prefix_restricts_the_walk() {
    let app = app_with(&[("tables/a.md", CONFORMANT), ("metrics/b.md", NO_TYPE)]).await;
    let (_, body) = send(
        &app,
        post("/v1/okf/notes/validate", r#"{"prefix":"tables/"}"#),
    )
    .await;
    assert_eq!(body["scanned"], 1);
    assert_eq!(body["conformant"], true);
}

#[tokio::test]
async fn test15_malformed_slug_returns_400_before_404() {
    let app = app_with(&[]).await;
    let (status, _) = send(&app, post("/v1/okf/NOT_A_SLUG/validate", "{}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test16_undeclared_kb_returns_404() {
    let app = app_with(&[]).await;
    let (status, body) = send(&app, post("/v1/okf/other/validate", "{}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not_found");
}

#[tokio::test]
async fn test17_missing_auth_returns_401() {
    let app = app_with(&[]).await;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/okf/notes/validate")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- computation contract ---

const COMPUTATION_INLINE: &str = "---\ntype: Attested Computation\n\
    title: Daily Revenue\nruntime: bigquery\n\
    parameters:\n  - {name: start_date, type: date, required: true}\n\
    executor: {resource: /executors/bq.md, receipt: [job_id]}\n\
    attester: {resource: /attesters/fin.md}\n\
    ---\n# Computation\n\n```sql\nSELECT 1;\n```\n";

#[tokio::test]
async fn test18_inline_computation_is_returned() {
    let app = app_with(&[("metrics/rev.md", COMPUTATION_INLINE)]).await;
    let (status, body) = send(&app, get("/v1/okf/notes/computation?path=metrics/rev.md")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["computation"]["source"], "inline");
    assert_eq!(body["computation"]["language"], "sql");
    assert_eq!(body["computation"]["code"], "SELECT 1;\n");
    assert_eq!(body["runtime"], "bigquery");
    assert_eq!(body["parameters"][0]["name"], "start_date");
}

#[tokio::test]
async fn test19_executor_and_attester_are_resolved_but_not_fetched() {
    let app = app_with(&[("metrics/rev.md", COMPUTATION_INLINE)]).await;
    let (_, body) = send(&app, get("/v1/okf/notes/computation?path=metrics/rev.md")).await;
    assert_eq!(body["executor"]["resource_path"], "executors/bq.md");
    assert_eq!(body["executor"]["receipt"][0], "job_id");
    assert_eq!(body["attester"]["resource_path"], "attesters/fin.md");
}

#[tokio::test]
async fn test20_the_response_states_the_non_execution_invariant() {
    let app = app_with(&[("metrics/rev.md", COMPUTATION_INLINE)]).await;
    let (_, body) = send(&app, get("/v1/okf/notes/computation?path=metrics/rev.md")).await;
    assert!(
        body["execution"]
            .as_str()
            .unwrap()
            .contains("never executes")
    );
}

#[tokio::test]
async fn test21_a_file_computation_is_fetched_from_storage() {
    let concept = "---\ntype: Attested Computation\nruntime: bigquery\n\
                   computation: ./rev.sql\n---\n# Notes\n";
    let app = app_with(&[
        ("metrics/rev.md", concept),
        ("metrics/rev.sql", "SELECT 2;\n"),
    ])
    .await;
    let (status, body) = send(&app, get("/v1/okf/notes/computation?path=metrics/rev.md")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["computation"]["source"], "file");
    assert_eq!(body["computation"]["code"], "SELECT 2;\n");
    assert_eq!(body["computation"]["origin"], "metrics/rev.sql");
    assert_eq!(body["computation"]["language"], "sql");
}

#[tokio::test]
async fn test22_an_absolute_url_computation_is_rejected_never_fetched() {
    // The SSRF guard: the server holds S3 credentials and sits inside the network.
    let concept = "---\ntype: Attested Computation\n\
                   computation: 'https://example.com/evil.sql'\n---\n";
    let app = app_with(&[("a.md", concept)]).await;
    let (status, body) = send(&app, get("/v1/okf/notes/computation?path=a.md")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("absolute URLs are never fetched")
    );
}

#[tokio::test]
async fn test23_a_traversing_computation_path_is_rejected() {
    let concept = "---\ntype: Attested Computation\ncomputation: ../../etc/passwd\n---\n";
    let app = app_with(&[("a.md", concept)]).await;
    let (status, _) = send(&app, get("/v1/okf/notes/computation?path=a.md")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test24_a_missing_computation_file_returns_404() {
    let concept = "---\ntype: Attested Computation\ncomputation: ./gone.sql\n---\n";
    let app = app_with(&[("a.md", concept)]).await;
    let (status, _) = send(&app, get("/v1/okf/notes/computation?path=a.md")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test25_a_concept_without_a_computation_returns_400() {
    let app = app_with(&[("a.md", CONFORMANT)]).await;
    let (status, _) = send(&app, get("/v1/okf/notes/computation?path=a.md")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test26_a_missing_concept_returns_404() {
    let app = app_with(&[]).await;
    let (status, _) = send(&app, get("/v1/okf/notes/computation?path=gone.md")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- browse ---

#[tokio::test]
async fn test27_index_is_parsed_with_resolved_paths() {
    let index = "# Tables\n\n* [Customers](./customers.md) - Customer master\n";
    let app = app_with(&[("tables/index.md", index)]).await;
    let (status, body) = send(&app, get("/v1/okf/notes/index?dir=tables")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "index.md");
    let entry = &body["sections"][0]["entries"][0];
    assert_eq!(entry["title"], "Customers");
    assert_eq!(entry["resolved_path"], "tables/customers.md");
    assert_eq!(entry["description"], "Customer master");
}

#[tokio::test]
async fn test28_a_missing_index_falls_back_to_a_listing() {
    let app = app_with(&[("tables/customers.md", CONFORMANT)]).await;
    let (status, body) = send(&app, get("/v1/okf/notes/index?dir=tables")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "listing");
    assert_eq!(body["sections"][0]["entries"][0]["title"], "customers.md");
}

#[tokio::test]
async fn test29_the_listing_fallback_collapses_subdirectories() {
    let app = app_with(&[("tables/deep/a.md", CONFORMANT)]).await;
    let (_, body) = send(&app, get("/v1/okf/notes/index?dir=tables")).await;
    let entry = &body["sections"][0]["entries"][0];
    assert_eq!(entry["title"], "deep");
    assert_eq!(entry["is_directory"], true);
}

#[tokio::test]
async fn test30_browsing_the_root_works_without_a_dir_parameter() {
    let app = app_with(&[("index.md", "# Root\n\n* [A](./a.md)\n")]).await;
    let (status, body) = send(&app, get("/v1/okf/notes/index")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sections"][0]["heading"], "Root");
}

// --- reindex ---

#[tokio::test]
async fn test31_reindex_defaults_to_a_dry_run() {
    let app = app_with(&[("tables/customers.md", CONFORMANT)]).await;
    let (status, body) = send(&app, post("/v1/okf/notes/reindex", r#"{"dir":"tables"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["applied"], false);
    assert_eq!(body["changed"], true);
    assert!(
        body["index_md"]
            .as_str()
            .unwrap()
            .contains("* [Customers](./customers.md)")
    );

    // Nothing was written.
    let (_, browse) = send(&app, get("/v1/okf/notes/index?dir=tables")).await;
    assert_eq!(browse["source"], "listing");
}

#[tokio::test]
async fn test32_reindex_writes_when_dry_run_is_false() {
    let app = app_with(&[("tables/customers.md", CONFORMANT)]).await;
    let (_, body) = send(
        &app,
        post(
            "/v1/okf/notes/reindex",
            r#"{"dir":"tables","dry_run":false}"#,
        ),
    )
    .await;
    assert_eq!(body["applied"], true);

    let (_, browse) = send(&app, get("/v1/okf/notes/index?dir=tables")).await;
    assert_eq!(browse["source"], "index.md");
    assert_eq!(browse["sections"][0]["heading"], "BigQuery Table");
}

#[tokio::test]
async fn test33_reindex_is_idempotent() {
    let app = app_with(&[("tables/customers.md", CONFORMANT)]).await;
    let body = r#"{"dir":"tables","dry_run":false}"#;
    send(&app, post("/v1/okf/notes/reindex", body)).await;
    let (_, second) = send(&app, post("/v1/okf/notes/reindex", body)).await;
    assert_eq!(second["changed"], false);
    assert_eq!(second["applied"], false);
}

#[tokio::test]
async fn test34_reindex_ignores_reserved_files_and_non_concepts() {
    let app = app_with(&[
        ("tables/customers.md", CONFORMANT),
        ("tables/log.md", "# Directory Update Log\n"),
        ("tables/notes.md", NO_FRONTMATTER),
    ])
    .await;
    let (_, body) = send(&app, post("/v1/okf/notes/reindex", r#"{"dir":"tables"}"#)).await;
    assert_eq!(body["entries"], 1);
}
