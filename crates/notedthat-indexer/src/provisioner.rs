//! Qdrant collection provisioning (§6.11) with idempotent create + manifest cross-check.

use crate::qdrant::QdrantWrapperError;
use crate::vector_store::{PayloadFieldKind, VectorStore, VectorStoreError};
use notedthat_core::{KbManifest, KbSlug, ManifestEmbedding};
use std::sync::Arc;

/// Errors returned by Qdrant collection provisioning and manifest cross-checks.
#[derive(Debug)]
pub enum ProvisionError {
    /// Error from the local Qdrant wrapper.
    Wrapper(QdrantWrapperError),
    /// Error returned by the Qdrant API.
    Qdrant {
        /// Knowledge-base slug.
        kb: String,
        /// Error message from Qdrant.
        source: String,
    },
    /// Stored manifest embedding configuration does not match current runtime configuration.
    ManifestMismatch {
        /// Knowledge-base slug.
        kb: String,
        /// Embedding model from manifest.
        manifest_model: String,
        /// Embedding dimension from manifest.
        manifest_dim: u32,
        /// Embedding model from environment.
        env_model: String,
        /// Embedding dimension from environment.
        env_dim: u32,
    },
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wrapper(err) => write!(f, "qdrant wrapper error: {err}"),
            Self::Qdrant { kb, source } => write!(f, "qdrant API error for kb={kb}: {source}"),
            Self::ManifestMismatch {
                kb,
                manifest_model,
                manifest_dim,
                env_model,
                env_dim,
            } => write!(
                f,
                "manifest embedding mismatch for kb={kb}: manifest={manifest_model}/{manifest_dim} env={env_model}/{env_dim}"
            ),
        }
    }
}

impl std::error::Error for ProvisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wrapper(err) => Some(err),
            Self::Qdrant { .. } | Self::ManifestMismatch { .. } => None,
        }
    }
}

impl From<QdrantWrapperError> for ProvisionError {
    fn from(value: QdrantWrapperError) -> Self {
        Self::Wrapper(value)
    }
}

/// Every payload index the search surface relies on.
///
/// Ensured on every startup, not only at collection-creation time — see
/// [`QdrantProvisioner::ensure_collection`].
const PAYLOAD_INDEXES: [(&str, PayloadFieldKind); 7] = [
    ("object_key", PayloadFieldKind::Keyword),
    ("etag", PayloadFieldKind::Keyword),
    ("mime", PayloadFieldKind::Keyword),
    ("mtime", PayloadFieldKind::Integer),
    ("heading_path", PayloadFieldKind::Keyword),
    ("tags", PayloadFieldKind::Keyword),
    ("okf.type", PayloadFieldKind::Keyword),
];

/// Provisions vector-store collection schema for a `NotedThat` knowledge base.
pub struct QdrantProvisioner {
    store: Arc<dyn VectorStore>,
}

impl QdrantProvisioner {
    /// Construct a provisioner around the shared vector store.
    pub fn new(store: Arc<dyn VectorStore>) -> Self {
        Self { store }
    }

    /// Ensure the Qdrant collection for `kb` exists with the expected schema.
    ///
    /// Idempotent. Collection creation is conditional, but **payload indexes are
    /// ensured on every call**, including for a collection that already exists.
    ///
    /// That last part fixes a real bug. This method used to return early when the
    /// collection existed, so a server upgraded in place never gained the payload
    /// indexes a new release added — and the remedy documented for the M4 → M5
    /// payload extension ("re-PUT your objects to backfill") could not help,
    /// because a re-PUT rewrites point payloads and does not create indexes. The
    /// only recovery was to delete the collection.
    pub async fn ensure_collection(
        &self,
        kb: &KbSlug,
        dense_dim: u64,
    ) -> Result<(), ProvisionError> {
        let exists = self
            .store
            .collection_exists(kb)
            .await
            .map_err(|err| provision_error(kb, &err))?;

        if !exists {
            self.store
                .create_collection(kb, dense_dim)
                .await
                .map_err(|err| provision_error(kb, &err))?;
        }

        // Deliberately no waiting: the backend builds payload indexes in the
        // background, and blocking startup until they are built would cost
        // minutes on a large collection — the opposite of D39's fail-fast
        // startup. Readers that need an index to exist poll for it.
        for (field, kind) in PAYLOAD_INDEXES {
            self.store
                .create_payload_index(kb, field, kind)
                .await
                .map_err(|err| provision_error(kb, &err))?;
        }

        Ok(())
    }

    /// Cross-check manifest's embedding config against current env config.
    ///
    /// Returns `Ok(None)` if manifest has no embedding and needs provisioning,
    /// `Ok(Some(()))` if manifest matches env, and `Err(ManifestMismatch)` if it disagrees.
    pub fn cross_check_manifest(
        manifest: &KbManifest,
        env_model: &str,
        env_dim: u32,
    ) -> Result<Option<()>, ProvisionError> {
        match &manifest.embedding {
            None => Ok(None),
            Some(m) if m.model == env_model && m.dimensions == env_dim => Ok(Some(())),
            Some(m) => Err(ProvisionError::ManifestMismatch {
                kb: manifest.kb_slug.as_str().to_string(),
                manifest_model: m.model.clone(),
                manifest_dim: m.dimensions,
                env_model: env_model.to_string(),
                env_dim,
            }),
        }
    }

    /// Build a `ManifestEmbedding` from env config for writing to manifest.
    pub fn manifest_embedding_from_env(
        model: String,
        dimensions: u32,
        endpoint_url_hint: Option<String>,
    ) -> ManifestEmbedding {
        ManifestEmbedding {
            model,
            dimensions,
            endpoint_url_hint,
        }
    }
}

/// Map a vector-store failure onto the provisioning error surface.
fn provision_error(kb: &KbSlug, err: &VectorStoreError) -> ProvisionError {
    ProvisionError::Qdrant {
        kb: kb.as_str().to_string(),
        source: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::{KbManifest, KbSlug, ManifestEmbedding, TenantSlug};

    fn make_manifest_no_embedding() -> KbManifest {
        KbManifest::new_v1(
            &TenantSlug::try_new("tenant").expect("valid tenant slug"),
            &KbSlug::try_new("test-kb").expect("valid kb slug"),
            "Test KB",
            1_700_000_000,
        )
    }

    fn make_manifest_with_embedding(model: &str, dim: u32) -> KbManifest {
        let mut manifest = make_manifest_no_embedding();
        manifest.embedding = Some(ManifestEmbedding {
            model: model.to_string(),
            dimensions: dim,
            endpoint_url_hint: None,
        });
        manifest
    }

    #[test]
    fn cross_check_no_embedding_returns_none() {
        let manifest = make_manifest_no_embedding();

        let result = QdrantProvisioner::cross_check_manifest(&manifest, "voyage-3", 1024);

        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn cross_check_matching_embedding_returns_some() {
        let manifest = make_manifest_with_embedding("voyage-3", 1024);

        let result = QdrantProvisioner::cross_check_manifest(&manifest, "voyage-3", 1024);

        assert!(matches!(result, Ok(Some(()))));
    }

    #[test]
    fn cross_check_model_mismatch_returns_error() {
        let manifest = make_manifest_with_embedding("voyage-3", 1024);

        let result =
            QdrantProvisioner::cross_check_manifest(&manifest, "text-embedding-3-small", 1024);

        assert!(matches!(
            result,
            Err(ProvisionError::ManifestMismatch { .. })
        ));
    }

    #[test]
    fn cross_check_dim_mismatch_returns_error() {
        let manifest = make_manifest_with_embedding("voyage-3", 1024);

        let result = QdrantProvisioner::cross_check_manifest(&manifest, "voyage-3", 1536);

        assert!(matches!(
            result,
            Err(ProvisionError::ManifestMismatch { .. })
        ));
    }

    #[test]
    fn cross_check_model_and_dim_mismatch_reports_manifest_and_env_values() {
        let manifest = make_manifest_with_embedding("voyage-3", 1024);

        let result =
            QdrantProvisioner::cross_check_manifest(&manifest, "text-embedding-3-small", 1536);

        match result {
            Err(ProvisionError::ManifestMismatch {
                kb,
                manifest_model,
                manifest_dim,
                env_model,
                env_dim,
            }) => {
                assert_eq!(kb, "test-kb");
                assert_eq!(manifest_model, "voyage-3");
                assert_eq!(manifest_dim, 1024);
                assert_eq!(env_model, "text-embedding-3-small");
                assert_eq!(env_dim, 1536);
            }
            other => panic!("expected ManifestMismatch, got {other:?}"),
        }
    }

    #[test]
    fn manifest_embedding_from_env_builds_struct() {
        let embedding = QdrantProvisioner::manifest_embedding_from_env(
            "voyage-3".to_string(),
            1024,
            Some("https://api.voyageai.com".to_string()),
        );

        assert_eq!(embedding.model, "voyage-3");
        assert_eq!(embedding.dimensions, 1024);
        assert_eq!(
            embedding.endpoint_url_hint,
            Some("https://api.voyageai.com".to_string())
        );
    }
}
