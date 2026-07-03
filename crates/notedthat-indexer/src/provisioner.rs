//! Qdrant collection provisioning (§6.11) with idempotent create + manifest cross-check.

use crate::qdrant::{QdrantClient, QdrantWrapperError};
use notedthat_core::{KbManifest, KbSlug, ManifestEmbedding};

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

/// Provisions Qdrant collection schema for a `NotedThat` knowledge base.
pub struct QdrantProvisioner {
    client: QdrantClient,
}

impl QdrantProvisioner {
    /// Construct a provisioner around the shared Qdrant client wrapper.
    pub fn new(client: QdrantClient) -> Self {
        Self { client }
    }

    /// Ensure the Qdrant collection for `kb` exists with the expected schema.
    ///
    /// Idempotent: checks existence first, creates only if absent.
    pub async fn ensure_collection(
        &self,
        kb: &KbSlug,
        dense_dim: u64,
    ) -> Result<(), ProvisionError> {
        let collection_name = format!("kb_{}_v1", kb.as_str());
        let inner = self.client.inner();

        let exists = inner
            .collection_exists(&collection_name)
            .await
            .map_err(|e| ProvisionError::Qdrant {
                kb: kb.as_str().to_string(),
                source: e.to_string(),
            })?;

        if exists {
            return Ok(());
        }

        self.create_collection_with_schema(inner, &collection_name, dense_dim)
            .await?;
        self.create_payload_indexes(inner, &collection_name, kb)
            .await?;

        Ok(())
    }

    async fn create_collection_with_schema(
        &self,
        inner: &qdrant_client::Qdrant,
        collection_name: &str,
        dense_dim: u64,
    ) -> Result<(), ProvisionError> {
        use qdrant_client::qdrant::{
            CreateCollectionBuilder, Distance, Modifier, SparseVectorParamsBuilder,
            SparseVectorsConfigBuilder, VectorParamsBuilder, VectorsConfigBuilder,
        };

        let mut vectors_config = VectorsConfigBuilder::default();
        vectors_config.add_named_vector_params(
            "dense",
            VectorParamsBuilder::new(dense_dim, Distance::Cosine),
        );

        let mut sparse_vectors_config = SparseVectorsConfigBuilder::default();
        sparse_vectors_config.add_named_vector_params(
            "sparse_bm25",
            SparseVectorParamsBuilder::default().modifier(Modifier::Idf),
        );

        let create = CreateCollectionBuilder::new(collection_name)
            .vectors_config(vectors_config)
            .sparse_vectors_config(sparse_vectors_config);

        inner
            .create_collection(create)
            .await
            .map_err(|e| ProvisionError::Qdrant {
                kb: collection_name.to_string(),
                source: e.to_string(),
            })?;

        Ok(())
    }

    async fn create_payload_indexes(
        &self,
        inner: &qdrant_client::Qdrant,
        collection_name: &str,
        kb: &KbSlug,
    ) -> Result<(), ProvisionError> {
        use qdrant_client::qdrant::{CreateFieldIndexCollectionBuilder, FieldType};

        for (field, ftype) in [
            ("object_key", FieldType::Keyword),
            ("etag", FieldType::Keyword),
            ("mtime", FieldType::Integer),
            ("heading_path", FieldType::Keyword),
        ] {
            inner
                .create_field_index(CreateFieldIndexCollectionBuilder::new(
                    collection_name,
                    field,
                    ftype,
                ))
                .await
                .map_err(|e| ProvisionError::Qdrant {
                    kb: kb.as_str().to_string(),
                    source: e.to_string(),
                })?;
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
