//! The `s3` backend's reconciliation pass (D67) against a real bucket: objects written
//! and deleted behind the server's back with the S3 API, then found by the operator's
//! pass and by the next startup.
//!
//! This is the only place the real `ListObjectsV2` `ETag` is proven end to end: a
//! regression to a listing without stamps shows here as `unchanged: 0` where `1` is
//! expected, because every object would count as changed.
//!
//! Needs Docker: `SeaweedFS` and Qdrant, exactly as `phase3_access_e2e`. Run with
//! `cargo test -p notedthat-server --test s3_reconcile_docker_e2e -- --include-ignored`.
#![allow(missing_docs)]

// The harness is shared with `phase3_access_e2e`, which uses more of it.
#[allow(dead_code)]
#[path = "support/access_env.rs"]
mod access_env;
#[allow(dead_code)]
#[path = "support/access_http.rs"]
mod access_http;
#[allow(dead_code)]
#[path = "support/access_server.rs"]
mod access_server;
#[allow(dead_code)]
#[path = "support/access_webdav.rs"]
mod access_webdav;
#[allow(dead_code)]
#[path = "support/access_wire.rs"]
mod access_wire;

use access_env::{API_TOKEN, Backends, PRIVATE_KB, PUBLIC_KB, kb};
use access_server::{ServerInstance, wait_ready};
use access_wire::{search, wait_indexed, wire};
use bytes::Bytes;
use notedthat_core::{
    AccessRule, ConditionalHeaders, KbManifest, ObjectPath, Storage, TenantSlug, Verb, Who,
};
use reqwest::StatusCode;
use std::time::Duration;

async fn store_manifests(backends: &Backends) {
    for slug in [PUBLIC_KB, PRIVATE_KB] {
        let kb = kb(slug);
        backends.storage.ensure_bucket(&kb).await.expect("bucket");
        let mut manifest = KbManifest::new_v1(&TenantSlug::default(), &kb, slug, 1_700_000_000);
        manifest.access = [AccessRule::new(Who::SignedIn, Verb::ALL)]
            .into_iter()
            .collect();
        backends
            .storage
            .write_manifest(&kb, &manifest)
            .await
            .expect("manifest");
    }
}

/// Write straight into the bucket, the way any S3 client would.
async fn put_behind_the_servers_back(backends: &Backends, key: &str, body: &str) {
    backends
        .storage
        .put_object(
            &kb(PUBLIC_KB),
            &ObjectPath::try_from(key).expect("key"),
            Bytes::from(body.to_string()),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("out-of-band put");
}

async fn index_health(client: &reqwest::Client, server: &ServerInstance) -> serde_json::Value {
    wire(
        "GET index health",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/index",
                server.http_url
            ))
            .bearer_auth(API_TOKEN)
            .send()
            .await
            .expect("index"),
    )
    .await
    .json()
}

/// Wait, bounded, for a completed pass other than `before`.
async fn wait_reconciled(
    client: &reqwest::Client,
    server: &ServerInstance,
    before: Option<&serde_json::Value>,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let health = index_health(client, server).await;
        let last = &health["last_reconcile"];
        if !last.is_null() && before != Some(last) {
            return last.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no reconciliation pass completed; /index is {health}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait, bounded, until a search for `key` no longer finds it.
async fn wait_forgotten(client: &reqwest::Client, server: &ServerInstance, key: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let response = search(client, server, key, Some(API_TOKEN)).await;
        assert_eq!(response.status, StatusCode::OK);
        let found = response.json()["hits"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["object_key"] == key));
        if !found {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{key} is still searchable after the pass"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn counts(last: &serde_json::Value) -> (u64, u64, u64, u64) {
    (
        last["objects_on_disk"].as_u64().unwrap(),
        last["unchanged"].as_u64().unwrap(),
        last["changed"].as_u64().unwrap(),
        last["orphaned"].as_u64().unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires Docker (persistent SeaweedFS + Qdrant testcontainers)"]
async fn out_of_band_bucket_changes_are_found_on_request_and_at_startup() {
    let backends = Backends::start().await;
    store_manifests(&backends).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("wire client");

    // Given: a running server that indexed `a.md` the ordinary way.
    let first = ServerInstance::start(backends.config());
    wait_ready(&client, &first).await;
    let response = wire(
        "HTTP PUT a.md",
        client
            .put(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/a.md",
                first.http_url
            ))
            .bearer_auth(API_TOKEN)
            .body("# A\n\nreconcile-needle-alpha")
            .send()
            .await
            .expect("put"),
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    wait_indexed(&client, &first, "a.md").await;
    let startup = wait_reconciled(&client, &first, None).await;

    // When: the bucket changes behind the server's back — one object arrives, one
    // leaves — and the operator asks for a pass.
    put_behind_the_servers_back(&backends, "b.md", "# B\n\nreconcile-needle-bravo").await;
    backends
        .storage
        .delete_object(
            &kb(PUBLIC_KB),
            &ObjectPath::try_from("a.md").expect("key"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("out-of-band delete");
    let response = wire(
        "HTTP POST index/reconcile",
        client
            .post(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/index/reconcile",
                first.http_url
            ))
            .bearer_auth(API_TOKEN)
            .send()
            .await
            .expect("post"),
    )
    .await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "{}", response.text());

    // Then: the new object is searchable, the deleted one is not, and the pass reported
    // exactly what it found — the `unchanged`/`changed` split is the real `ETag` at work.
    wait_indexed(&client, &first, "b.md").await;
    wait_forgotten(&client, &first, "a.md").await;
    let requested = wait_reconciled(&client, &first, Some(&startup)).await;
    assert_eq!(counts(&requested), (1, 0, 1, 1), "{requested}");

    // A pass with nothing to do says so, and reads no object.
    let response = client
        .post(format!(
            "{}/api/v1/knowledgebases/{PUBLIC_KB}/index/reconcile",
            first.http_url
        ))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .expect("post");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let clean = wait_reconciled(&client, &first, Some(&requested)).await;
    assert_eq!(counts(&clean), (1, 1, 0, 0), "{clean}");

    // And: an object written while the server is down is found by the next startup.
    first.stop();
    put_behind_the_servers_back(&backends, "c.md", "# C\n\nreconcile-needle-charlie").await;
    let second = ServerInstance::start(backends.config());
    wait_ready(&client, &second).await;
    wait_indexed(&client, &second, "c.md").await;
    let boot = wait_reconciled(&client, &second, None).await;
    assert_eq!(counts(&boot), (2, 1, 1, 0), "{boot}");
    second.stop();

    backends.remove().await;
}
