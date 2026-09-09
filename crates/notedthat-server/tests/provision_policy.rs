//! Startup manifest-policy loading tests.

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{AccessPolicy, KbManifest, KbSlug, Principal, Storage, TenantSlug, Verb};
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
        "access": [{ "who": "anyone", "may": ["read"] }]
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
            .allows(Principal::Anyone, Verb::Read, "public.md")
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
    //
    // The failure now arrives from `read_manifest` rather than from `provision_kbs`'s own
    // `manifest.validate()`, because every backend validates the schema on read and
    // reports an unsupported one as `BackendUnavailable` — `S3Storage` has always done
    // so, and the substitute used to differ. `provision_kbs` keeps its own check for a
    // backend that does not. What matters for D39 is unchanged: startup stops.
    assert!(
        result.is_err(),
        "an unsupported manifest schema must abort startup"
    );
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
        "access": [{ "who": "anyone", "may": ["read"] }]
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
    manifest.access = AccessPolicy::empty();
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
            .allows(Principal::Anyone, Verb::Read, "public.md")
    );
    assert!(
        !restarted_snapshot
            .get("notes")
            .expect("restarted policy exists")
            .allows(Principal::Anyone, Verb::Read, "public.md"),
        "a restart is what picks up a manifest edit"
    );
}

#[tokio::test]
async fn a_manifest_still_declaring_public_read_loads_with_no_anonymous_access() {
    // Given: a manifest written before D50, carrying the removed `public_read`
    // field. The field is gone from the struct, serde ignores unknown keys, and
    // no `access` field means the default — so the knowledge base comes up with
    // its credentialed reach intact and its public grants silently gone.
    //
    // This is a deliberate migration choice, not an oversight, and it is exactly
    // the sort of thing that should be pinned rather than remembered: an
    // operator upgrading a public knowledge base gets a private one, and the
    // release notes are the only warning.
    let storage = InMemoryStorage::default();
    let tenant = TenantSlug::default();
    let kb = KbSlug::try_new("notes").expect("valid slug");
    storage.ensure_bucket(&kb).await.expect("bucket created");
    let manifest: KbManifest = serde_json::from_value(serde_json::json!({
        "notedthat_version": "0.3.1",
        "manifest_version": 1,
        "tenant_slug": "default",
        "kb_slug": "notes",
        "display_name": "Notes",
        "created_at": 1_700_000_000_i64,
        "public_read": ["discover", "browse", "content", "search"]
    }))
    .expect("a pre-D50 manifest still parses");
    storage
        .write_manifest(&kb, &manifest)
        .await
        .expect("manifest stored");

    // When
    let policies = provision_kbs(
        &storage,
        &tenant,
        std::slice::from_ref(&kb),
        &provisioner(),
        "test-model",
        3,
        None,
    )
    .await
    .expect("provisioning succeeds rather than refusing the old field");

    // Then
    let policy = policies.get("notes").expect("declared KB has a policy");
    for verb in Verb::ALL {
        assert!(
            !policy.allows(Principal::Anyone, verb, "public.md"),
            "the removed field must grant nothing: {verb:?}"
        );
    }
    assert!(
        policy.allows(Principal::SignedIn, Verb::Write, "public.md"),
        "credentialed access must survive the upgrade untouched"
    );
}
