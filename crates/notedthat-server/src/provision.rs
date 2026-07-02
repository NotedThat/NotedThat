//! Startup provisioning: validate configs, ensure buckets, write manifests.
//!
//! See `SPECIFICATIONS.md` §6.12 provisioning steps and D39 (fail-fast startup).

use notedthat_core::{
    Error, KbManifest, KbSlug, Storage, TenantSlug, derive_bucket_name, validate_bucket_name,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Provision all declared knowledge bases against the storage backend.
///
/// For each KB:
/// 1. Validate the derived bucket name (fail-fast on >63 char names).
/// 2. Ensure the bucket exists (idempotent).
/// 3. Read the manifest; if absent, write a fresh v1 manifest.
///    If present but KB slug mismatches, overwrite with a fresh manifest.
///
/// Any failure returns immediately as `Err(Error)`.
pub async fn provision_kbs(
    storage: &dyn Storage,
    tenant: &TenantSlug,
    kbs: &[KbSlug],
) -> Result<(), Error> {
    for kb in kbs {
        validate_bucket_name(tenant, kb)?;

        let bucket = derive_bucket_name(tenant, kb);
        info!(kb = %kb.as_str(), bucket = %bucket, "provisioning KB");

        storage.ensure_bucket(kb).await?;

        match storage.read_manifest(kb).await {
            Ok(manifest) => {
                if manifest.kb_slug.as_str() != kb.as_str() {
                    warn!(
                        kb = %kb.as_str(),
                        manifest_kb = %manifest.kb_slug.as_str(),
                        "manifest kb_slug mismatch — overwriting with fresh manifest"
                    );
                    let fresh = KbManifest::new_v1(tenant, kb, kb.as_str(), current_unix_ts());
                    storage.write_manifest(kb, &fresh).await?;
                }
                info!(kb = %kb.as_str(), "manifest OK");
            }
            Err(e) if e.is_not_found() => {
                info!(kb = %kb.as_str(), "writing initial manifest");
                let fresh = KbManifest::new_v1(tenant, kb, kb.as_str(), current_unix_ts());
                storage.write_manifest(kb, &fresh).await?;
            }
            Err(e) => return Err(Error::Storage(e)),
        }
    }
    Ok(())
}

/// Current Unix timestamp in seconds. Used for `created_at` in manifests.
/// No chrono dependency — [`SystemTime`] from std is sufficient for M2.
fn current_unix_ts() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_api_http::testing::InMemoryStorage;

    #[tokio::test]
    async fn test_provision_kbs_happy_path() {
        let storage = InMemoryStorage::default();
        let tenant = TenantSlug::default();
        let kbs = vec![
            KbSlug::try_new("notes").unwrap(),
            KbSlug::try_new("docs").unwrap(),
        ];

        provision_kbs(&storage, &tenant, &kbs).await.unwrap();

        assert_eq!(
            storage
                .read_manifest(&kbs[0])
                .await
                .unwrap()
                .kb_slug
                .as_str(),
            "notes"
        );
        assert_eq!(
            storage
                .read_manifest(&kbs[1])
                .await
                .unwrap()
                .kb_slug
                .as_str(),
            "docs"
        );
    }

    #[tokio::test]
    async fn test_provision_kbs_idempotent() {
        let storage = InMemoryStorage::default();
        let tenant = TenantSlug::default();
        let kbs = vec![KbSlug::try_new("notes").unwrap()];

        provision_kbs(&storage, &tenant, &kbs).await.unwrap();
        provision_kbs(&storage, &tenant, &kbs).await.unwrap();

        let manifest = storage.read_manifest(&kbs[0]).await.unwrap();
        assert_eq!(manifest.kb_slug.as_str(), "notes");
        assert_eq!(manifest.manifest_version, KbManifest::CURRENT_VERSION);
    }

    #[tokio::test]
    async fn test_provision_kbs_writes_initial_manifest() {
        let storage = InMemoryStorage::default();
        let tenant = TenantSlug::default();
        let kb = KbSlug::try_new("mykb").unwrap();
        let kbs = vec![kb.clone()];

        provision_kbs(&storage, &tenant, &kbs).await.unwrap();

        let manifest = storage.read_manifest(&kb).await.unwrap();
        assert_eq!(manifest.kb_slug.as_str(), "mykb");
        assert_eq!(manifest.tenant_slug.as_str(), "default");
        assert_eq!(manifest.manifest_version, KbManifest::CURRENT_VERSION);
        assert!(manifest.created_at > 0);
    }
}
