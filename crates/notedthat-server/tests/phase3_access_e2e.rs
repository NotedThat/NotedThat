#![allow(missing_docs)]

#[path = "support/access_env.rs"]
mod access_env;
#[path = "support/access_http.rs"]
mod access_http;
#[path = "support/access_server.rs"]
mod access_server;
#[path = "support/access_webdav.rs"]
mod access_webdav;
#[path = "support/access_wire.rs"]
mod access_wire;

use access_env::{
    API_TOKEN, Backends, INTERNAL_BODY, PRIVATE_BODY, PRIVATE_KB, PUBLIC_BODY, PUBLIC_KB, kb,
    stored_manifest,
};
use access_server::{ServerInstance, wait_ready};
use access_wire::{assert_http_401, search, wait_indexed, wire};
use notedthat_core::{AccessPolicy, AccessRule, KbManifest, Principal, Storage, TenantSlug, Verb};
use reqwest::StatusCode;
use std::time::Duration;

/// A policy granting `anonymous_verbs` to anyone, alongside the credential
/// holder's usual full reach.
fn access_policy(anonymous_verbs: Vec<Verb>) -> AccessPolicy {
    let mut rules = vec![AccessRule::new(Principal::SignedIn, Verb::ALL)];
    if !anonymous_verbs.is_empty() {
        rules.push(AccessRule::new(Principal::Anyone, anonymous_verbs));
    }
    rules.into_iter().collect()
}

async fn store_initial_manifests(backends: &Backends) {
    for (slug, anonymous_verbs) in [(PUBLIC_KB, vec![Verb::Read]), (PRIVATE_KB, Vec::new())] {
        let kb = kb(slug);
        backends
            .storage
            .ensure_bucket(&kb)
            .await
            .expect("KB bucket");
        let mut manifest = KbManifest::new_v1(&TenantSlug::default(), &kb, slug, 1_700_000_000);
        manifest.access = access_policy(anonymous_verbs);
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
                .put(format!(
                    "{}/api/v1/knowledgebases/{kb}/{path}",
                    server.http_url
                ))
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

/// What the first startup snapshot grants: `read` on `PUBLIC_KB` and nothing else.
async fn verify_first_snapshot(client: &reqwest::Client, first: &ServerInstance) {
    let discovery = wire(
        "HTTP anonymous discovery before restart",
        client
            .get(format!("{}/api/v1/knowledgebases", first.http_url))
            .send()
            .await
            .expect("discovery"),
    )
    .await;
    // D50: visibility is derived from holding a grant, so PUBLIC_KB's `read`
    // rule is enough to name it — there is no separate `discover` to withhold.
    // PRIVATE_KB grants anonymous callers nothing and stays absent.
    assert_eq!(discovery.status, StatusCode::OK);
    assert!(
        discovery.text().contains(PUBLIC_KB),
        "a knowledge base granting `read` is visible: {}",
        discovery.text()
    );
    assert!(
        !discovery.text().contains(PRIVATE_KB),
        "a knowledge base granting nothing is not: {}",
        discovery.text()
    );
    let content = wire(
        "HTTP anonymous content before restart",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                first.http_url
            ))
            .send()
            .await
            .expect("content"),
    )
    .await;
    assert_eq!(content.status, StatusCode::OK);
    assert_eq!(content.text(), PUBLIC_BODY);
    assert_http_401(&search(client, first, "public.md", None).await);
    for path in [
        format!("{}/api/v1/knowledgebases/{PUBLIC_KB}", first.http_url),
        format!(
            "{}/api/v1/knowledgebases/{PRIVATE_KB}/private.md",
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
}

#[tokio::test]
#[ignore = "requires Docker (persistent SeaweedFS + Qdrant testcontainers)"]
async fn stored_access_rules_are_loaded_only_at_server_startup() {
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
    verify_first_snapshot(&client, &first).await;

    let mut public_manifest = stored_manifest(&backends.storage, PUBLIC_KB).await;
    public_manifest.access = access_policy(vec![Verb::List, Verb::Read, Verb::Search]);
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
            .get(format!("{}/api/v1/knowledgebases", first.http_url))
            .send()
            .await
            .expect("stale discovery"),
    )
    .await;
    // The stored manifest now grants `list` and `search` too, but this server
    // loaded its policy at startup and never re-reads it — so the *new* grants
    // must not be in force. Discovery still answers from the old snapshot, where
    // `read` alone already made the knowledge base visible; what proves the edit
    // has not leaked is that listing and search are still refused below.
    assert_eq!(stale.status, StatusCode::OK);
    assert_http_401(
        &wire(
            "HTTP stale listing after live manifest edit",
            client
                .get(format!(
                    "{}/api/v1/knowledgebases/{PUBLIC_KB}",
                    first.http_url
                ))
                .send()
                .await
                .expect("stale listing"),
        )
        .await,
    );
    assert_http_401(&search(&client, &first, "public.md", None).await);
    first.stop();

    let second = ServerInstance::start(backends.config());
    wait_ready(&client, &second).await;

    // Then: the restarted real server exposes only granted reads and no writes.
    access_http::verify(&client, &second).await;
    access_webdav::verify(&client, &second).await;
    second.stop();
    backends.remove().await;
}
