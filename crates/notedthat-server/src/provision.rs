//! Startup provisioning: validate configs, ensure buckets, write manifests.
//!
//! See `SPECIFICATIONS.md` §6.12 provisioning steps and D39 (fail-fast startup).

use notedthat_core::{
    Error, KbManifest, KbSlug, Storage, TenantSlug, derive_bucket_name, validate_bucket_name,
};
use notedthat_indexer::{ProvisionError, QdrantProvisioner};
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
    provisioner: &QdrantProvisioner,
    embedder_model: &str,
    embedder_dim: u32,
    embedder_endpoint_hint: Option<&str>,
) -> Result<(), Error> {
    for kb in kbs {
        validate_bucket_name(tenant, kb)?;

        let bucket = derive_bucket_name(tenant, kb);
        info!(kb = %kb.as_str(), bucket = %bucket, "provisioning KB");

        storage.ensure_bucket(kb).await?;

        let mut manifest = match storage.read_manifest(kb).await {
            Ok(manifest) => {
                if manifest.kb_slug.as_str() == kb.as_str() {
                    info!(kb = %kb.as_str(), "manifest OK");
                    manifest
                } else {
                    warn!(
                        kb = %kb.as_str(),
                        manifest_kb = %manifest.kb_slug.as_str(),
                        "manifest kb_slug mismatch — overwriting with fresh manifest"
                    );
                    let fresh = KbManifest::new_v1(tenant, kb, kb.as_str(), current_unix_ts());
                    storage.write_manifest(kb, &fresh).await?;
                    fresh
                }
            }
            Err(e) if e.is_not_found() => {
                info!(kb = %kb.as_str(), "writing initial manifest");
                let fresh = KbManifest::new_v1(tenant, kb, kb.as_str(), current_unix_ts());
                storage.write_manifest(kb, &fresh).await?;
                fresh
            }
            Err(e) => return Err(Error::Storage(e)),
        };

        match QdrantProvisioner::cross_check_manifest(&manifest, embedder_model, embedder_dim) {
            Ok(None) => match provisioner
                .ensure_collection(kb, u64::from(embedder_dim))
                .await
            {
                Ok(()) => {
                    manifest.embedding = Some(QdrantProvisioner::manifest_embedding_from_env(
                        embedder_model.to_string(),
                        embedder_dim,
                        embedder_endpoint_hint.map(str::to_string),
                    ));
                    storage.write_manifest(kb, &manifest).await?;
                    info!(kb = %kb.as_str(), "qdrant collection provisioned and manifest embedding recorded");
                }
                Err(err) => warn!(
                    kb = %kb.as_str(),
                    error = %err,
                    "qdrant collection provisioning failed; continuing startup"
                ),
            },
            Ok(Some(())) => {
                if let Err(err) = provisioner
                    .ensure_collection(kb, u64::from(embedder_dim))
                    .await
                {
                    warn!(
                        kb = %kb.as_str(),
                        error = %err,
                        "qdrant collection ensure failed; continuing startup"
                    );
                }
            }
            Err(err @ ProvisionError::ManifestMismatch { .. }) => {
                return Err(provision_error(&err));
            }
            Err(err) => warn!(
                kb = %kb.as_str(),
                error = %err,
                "qdrant manifest cross-check failed; continuing startup"
            ),
        }
    }
    Ok(())
}

fn provision_error(err: &ProvisionError) -> Error {
    Error::Config {
        message: err.to_string(),
    }
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
    use notedthat_indexer::{QdrantClient, QdrantConfig};

    fn provisioner() -> QdrantProvisioner {
        QdrantProvisioner::new(
            QdrantClient::new(&QdrantConfig {
                url: "http://127.0.0.1:6334".to_string(),
                api_key: None,
            })
            .expect("qdrant client construction does not connect"),
        )
    }

    #[tokio::test]
    async fn test_provision_kbs_happy_path() {
        let storage = InMemoryStorage::default();
        let tenant = TenantSlug::default();
        let kbs = vec![
            KbSlug::try_new("notes").unwrap(),
            KbSlug::try_new("docs").unwrap(),
        ];

        provision_kbs(
            &storage,
            &tenant,
            &kbs,
            &provisioner(),
            "test-model",
            3,
            Some("http://embedder.example"),
        )
        .await
        .unwrap();

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

        provision_kbs(
            &storage,
            &tenant,
            &kbs,
            &provisioner(),
            "test-model",
            3,
            Some("http://embedder.example"),
        )
        .await
        .unwrap();
        provision_kbs(
            &storage,
            &tenant,
            &kbs,
            &provisioner(),
            "test-model",
            3,
            Some("http://embedder.example"),
        )
        .await
        .unwrap();

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

        provision_kbs(
            &storage,
            &tenant,
            &kbs,
            &provisioner(),
            "test-model",
            3,
            Some("http://embedder.example"),
        )
        .await
        .unwrap();

        let manifest = storage.read_manifest(&kb).await.unwrap();
        assert_eq!(manifest.kb_slug.as_str(), "mykb");
        assert_eq!(manifest.tenant_slug.as_str(), "default");
        assert_eq!(manifest.manifest_version, KbManifest::CURRENT_VERSION);
        assert!(manifest.created_at > 0);
    }
}
