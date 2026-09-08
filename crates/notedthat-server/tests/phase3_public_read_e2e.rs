#![allow(missing_docs)]

#[path = "support/public_read_env.rs"]
mod public_read_env;
#[path = "support/public_read_http.rs"]
mod public_read_http;
#[path = "support/public_read_server.rs"]
mod public_read_server;
#[path = "support/public_read_webdav.rs"]
mod public_read_webdav;
#[path = "support/public_read_wire.rs"]
mod public_read_wire;

use notedthat_core::{KbManifest, PublicReadCapability, Storage, TenantSlug};
use public_read_env::{
    API_TOKEN, Backends, INTERNAL_BODY, PRIVATE_BODY, PRIVATE_KB, PUBLIC_BODY, PUBLIC_KB, kb,
    stored_manifest,
};
use public_read_server::{ServerInstance, wait_ready};
use public_read_wire::{assert_http_401, search, wait_indexed, wire};
use reqwest::StatusCode;
use std::time::Duration;

async fn store_initial_manifests(backends: &Backends) {
    for (slug, capabilities) in [
        (PUBLIC_KB, vec![PublicReadCapability::Content]),
        (PRIVATE_KB, Vec::new()),
    ] {
        let kb = kb(slug);
        backends
            .storage
            .ensure_bucket(&kb)
            .await
            .expect("KB bucket");
        let mut manifest = KbManifest::new_v1(&TenantSlug::default(), &kb, slug, 1_700_000_000);
        manifest.public_read = capabilities.into_iter().collect();
        backends
            .storage
            .write_manifest(&kb, &manifest)
            .await
            .expect("stored fixture manifest");
    }
}

async fn seed_objects(client: &reqwest::Client, server: &ServerInstance) {
    for (kb, path, body) in [
        (PUBLIC_KB, "public.md", PUBLIC_BODY),
        (PRIVATE_KB, "private.md", PRIVATE_BODY),
        (PUBLIC_KB, ".notedthat/leak.md", INTERNAL_BODY),
    ] {
        let response = wire(
            "authenticated HTTP PUT seed",
            client
                .put(format!("{}/v1/knowledgebases/{kb}/{path}", server.http_url))
                .bearer_auth(API_TOKEN)
                .body(body)
                .send()
                .await
                .expect("seed response"),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        if kb == PUBLIC_KB {
            wait_indexed(client, server, path).await;
        }
    }
}

#[tokio::test]
#[ignore = "requires Docker (persistent SeaweedFS + Qdrant testcontainers)"]
async fn stored_public_read_policy_is_loaded_only_at_server_startup() {
    // Given: persistent backends hold one private and one content-only manifest.
    let backends = Backends::start().await;
    store_initial_manifests(&backends).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("wire client");
    let first = ServerInstance::start(backends.config());
    wait_ready(&client, &first).await;

    seed_objects(&client, &first).await;

    // When: requests hit the first startup snapshot, then its stored manifest changes.
    let discovery = wire(
        "HTTP anonymous discovery before restart",
        client
            .get(format!("{}/v1/knowledgebases", first.http_url))
            .send()
            .await
            .expect("discovery"),
    )
    .await;
    assert_eq!(discovery.json()["knowledgebases"], serde_json::json!([]));
    let content = wire(
        "HTTP anonymous content before restart",
        client
            .get(format!(
                "{}/v1/knowledgebases/{PUBLIC_KB}/public.md",
                first.http_url
            ))
            .send()
            .await
            .expect("content"),
    )
    .await;
    assert_eq!(content.status, StatusCode::OK);
    assert_eq!(content.text(), PUBLIC_BODY);
    assert_http_401(&search(&client, &first, "public.md", None).await);
    for path in [
        format!("{}/v1/knowledgebases/{PUBLIC_KB}", first.http_url),
        format!(
            "{}/v1/knowledgebases/{PRIVATE_KB}/private.md",
            first.http_url
        ),
    ] {
        assert_http_401(
            &wire(
                "HTTP anonymous denied before restart",
                client.get(path).send().await.expect("denied response"),
            )
            .await,
        );
    }

    let mut public_manifest = stored_manifest(&backends.storage, PUBLIC_KB).await;
    public_manifest.public_read = [
        PublicReadCapability::Discover,
        PublicReadCapability::Browse,
        PublicReadCapability::Content,
        PublicReadCapability::Search,
    ]
    .into_iter()
    .collect();
    backends
        .storage
        .write_manifest(&kb(PUBLIC_KB), &public_manifest)
        .await
        .expect("update stored public manifest");
    println!(
        "STORED manifest public={} private={}",
        serde_json::to_string(&public_manifest).expect("public manifest JSON"),
        serde_json::to_string(&stored_manifest(&backends.storage, PRIVATE_KB).await)
            .expect("private manifest JSON")
    );
    let stale = wire(
        "HTTP stale discovery after live manifest edit",
        client
            .get(format!("{}/v1/knowledgebases", first.http_url))
            .send()
            .await
            .expect("stale discovery"),
    )
    .await;
    assert_eq!(stale.json()["knowledgebases"], serde_json::json!([]));
    assert_http_401(&search(&client, &first, "public.md", None).await);
    first.stop();

    let second = ServerInstance::start(backends.config());
    wait_ready(&client, &second).await;

    // Then: the restarted real server exposes only granted reads and no writes.
    public_read_http::verify(&client, &second).await;
    public_read_webdav::verify(&client, &second).await;
    second.stop();
    backends.remove().await;
}
