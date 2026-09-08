//! Manifest public-read policy boundary tests.

use notedthat_core::{KbManifest, PublicReadCapability, PublicReadPolicy};
use serde_json::json;

fn manifest_json() -> serde_json::Value {
    json!({
        "notedthat_version": "0.1.6",
        "manifest_version": 1,
        "tenant_slug": "default",
        "kb_slug": "notes",
        "display_name": "Notes",
        "created_at": 1_700_000_000_i64
    })
}

#[test]
fn missing_public_read_is_private() {
    // Given: a v1 manifest with no public-read declaration.
    let json = manifest_json();

    // When: the manifest crosses the JSON boundary.
    let manifest: KbManifest = serde_json::from_value(json).expect("valid manifest");

    // Then: no anonymous capability is granted.
    assert!(manifest.public_read.is_private());
}

#[test]
fn empty_public_read_is_private() {
    // Given: a v1 manifest with an explicitly empty public-read declaration.
    let mut json = manifest_json();
    json["public_read"] = json!([]);

    // When: the manifest crosses the JSON boundary.
    let manifest: KbManifest = serde_json::from_value(json).expect("valid manifest");

    // Then: no anonymous capability is granted.
    assert!(manifest.public_read.is_private());
}

#[test]
fn public_read_duplicates_deduplicate_and_serialize_deterministically() {
    // Given: all capabilities in a non-canonical order, including a duplicate.
    let mut json = manifest_json();
    json["public_read"] = json!(["search", "content", "discover", "browse", "content"]);

    // When: the manifest is parsed and serialized.
    let manifest: KbManifest = serde_json::from_value(json).expect("valid manifest");
    let serialized = serde_json::to_value(&manifest).expect("serializable manifest");

    // Then: membership is typed and output has one stable ordering.
    assert!(manifest.public_read.allows(PublicReadCapability::Content));
    assert_eq!(
        serialized["public_read"],
        json!(["discover", "browse", "content", "search"])
    );
}

#[test]
fn unknown_public_read_capability_is_rejected() {
    // Given: a manifest declaring a capability outside the v1 vocabulary.
    let mut json = manifest_json();
    json["public_read"] = json!(["download"]);

    // When: the manifest crosses the JSON boundary.
    let result = serde_json::from_value::<KbManifest>(json);

    // Then: the unrecognized capability is rejected.
    assert!(result.is_err());
}

#[test]
fn non_array_public_read_is_rejected() {
    // Given: a manifest with the wrong JSON shape for public_read.
    let mut json = manifest_json();
    json["public_read"] = json!({ "content": true });

    // When: the manifest crosses the JSON boundary.
    let result = serde_json::from_value::<KbManifest>(json);

    // Then: the malformed declaration is rejected.
    assert!(result.is_err());
}

#[test]
fn new_manifest_is_private_and_omits_empty_public_read() {
    // Given: a freshly provisioned manifest.
    let manifest = KbManifest::new_v1(
        &notedthat_core::TenantSlug::default(),
        &notedthat_core::KbSlug::try_new("notes").expect("valid slug"),
        "Notes",
        1_700_000_000,
    );

    // When: its policy and JSON representation are observed.
    let serialized = serde_json::to_value(&manifest).expect("serializable manifest");

    // Then: it remains private and backward-compatible on disk.
    assert_eq!(manifest.public_read, PublicReadPolicy::default());
    assert!(serialized.get("public_read").is_none());
}

#[test]
fn public_read_policy_collects_typed_capabilities() {
    // Given: typed capabilities containing a duplicate in non-canonical order.
    let capabilities = [
        PublicReadCapability::Content,
        PublicReadCapability::Discover,
        PublicReadCapability::Content,
    ];

    // When: callers collect them into a public-read policy.
    let policy: PublicReadPolicy = capabilities.into_iter().collect();

    // Then: the policy deduplicates and retains deterministic capability ordering.
    assert_eq!(
        serde_json::to_value(policy).expect("serializable policy"),
        json!(["discover", "content"])
    );
}
