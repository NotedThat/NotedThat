//! Startup manifest-policy loading tests.

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{
    Error, KbManifest, KbSlug, PublicReadCapability, PublicReadPolicy, Storage, TenantSlug,
};
use notedthat_indexer::{QdrantClient, QdrantConfig, QdrantProvisioner, VectorStore};
use notedthat_server::provision::provision_kbs;
use std::sync::Arc;

/// A provisioner over a real `QdrantClient` that is never reached.
///
/// These tests only exercise the manifest half of `provision_kbs`, which
/// returns before any collection call. The client is kept concrete rather than
/// swapped for `InMemoryVectorStore` so the test would fail loudly if that stopped
/// being true, instead of silently starting to assert in-memory behaviour.
fn provisioner() -> QdrantProvisioner {
    let store: Arc<dyn VectorStore> = Arc::new(
        QdrantClient::new(&QdrantConfig {
            url: "http://127.0.0.1:6334".to_string(),
            api_key: None,
            ..Default::default()
        })
        .expect("qdrant client construction does not connect"),
    );
    QdrantProvisioner::new(store)
}

#[tokio::test]
async fn provision_kbs_returns_policy_loaded_from_existing_manifest() {
    // Given: storage has a concrete manifest granting anonymous content reads.
    let storage = InMemoryStorage::default();
    let tenant = TenantSlug::default();
    let kb = KbSlug::try_new("notes").expect("valid slug");
    storage.ensure_bucket(&kb).await.expect("bucket created");
    let manifest: KbManifest = serde_json::from_value(serde_json::json!({
        "notedthat_version": "0.1.6",
        "manifest_version": 1,
        "tenant_slug": "default",
        "kb_slug": "notes",
        "display_name": "Notes",
        "created_at": 1_700_000_000_i64,
        "public_read": ["content"]
    }))
    .expect("valid public manifest");
    storage
        .write_manifest(&kb, &manifest)
        .await
        .expect("manifest stored");

    // When: startup provisioning loads the declared knowledge base.
    let policies = provision_kbs(
        &storage,
        &tenant,
        std::slice::from_ref(&kb),
        &provisioner(),
        "test-model",
        3,
        Some("http://embedder.example"),
    )
    .await
    .expect("provisioning succeeds");

    // Then: the validated policy is returned for immutable state sharing.
    assert!(
        policies
            .get("notes")
            .expect("declared KB has a policy")
            .allows(PublicReadCapability::Content)
    );
}

#[tokio::test]
async fn provision_kbs_rejects_unsupported_manifest_before_returning_policies() {
    // Given: storage has a manifest whose schema version is unsupported.
    let storage = InMemoryStorage::default();
    let tenant = TenantSlug::default();
    let kb = KbSlug::try_new("notes").expect("valid slug");
    storage.ensure_bucket(&kb).await.expect("bucket created");
    let mut manifest = KbManifest::new_v1(&tenant, &kb, "Notes", 1_700_000_000);
    manifest.manifest_version = 99;
    storage
        .write_manifest(&kb, &manifest)
        .await
        .expect("manifest stored");

    // When: startup provisioning attempts to load the manifest policy.
    let result = provision_kbs(
        &storage,
        &tenant,
        std::slice::from_ref(&kb),
        &provisioner(),
        "test-model",
        3,
        Some("http://embedder.example"),
    )
    .await;

    // Then: startup fails rather than publishing policy from an unsupported schema.
    assert!(matches!(result, Err(Error::InvalidInput { .. })));
}

#[tokio::test]
async fn provision_kbs_refreshes_policy_only_when_provisioning_runs_again() {
    // Given: one provisioning snapshot loaded a public manifest that storage later makes private.
    let storage = InMemoryStorage::default();
    let tenant = TenantSlug::default();
    let kb = KbSlug::try_new("notes").expect("valid slug");
    storage.ensure_bucket(&kb).await.expect("bucket created");
    let mut manifest: KbManifest = serde_json::from_value(serde_json::json!({
        "notedthat_version": "0.1.6",
        "manifest_version": 1,
        "tenant_slug": "default",
        "kb_slug": "notes",
        "display_name": "Notes",
        "created_at": 1_700_000_000_i64,
        "public_read": ["content"]
    }))
    .expect("valid public manifest");
    storage
        .write_manifest(&kb, &manifest)
        .await
        .expect("manifest stored");
    let first_snapshot = provision_kbs(
        &storage,
        &tenant,
        std::slice::from_ref(&kb),
        &provisioner(),
        "test-model",
        3,
        None,
    )
    .await
    .expect("initial provisioning succeeds");
    manifest.public_read = PublicReadPolicy::default();
    storage
        .write_manifest(&kb, &manifest)
        .await
        .expect("private manifest stored");

    // When: provisioning runs again, as it does on server restart.
    let restarted_snapshot = provision_kbs(
        &storage,
        &tenant,
        std::slice::from_ref(&kb),
        &provisioner(),
        "test-model",
        3,
        None,
    )
    .await
    .expect("restart provisioning succeeds");

    // Then: the old snapshot is stable and only the restarted snapshot observes the edit.
    assert!(
        first_snapshot
            .get("notes")
            .expect("first policy exists")
            .allows(PublicReadCapability::Content)
    );
    assert!(
        restarted_snapshot
            .get("notes")
            .expect("restarted policy exists")
            .is_private()
    );
}
