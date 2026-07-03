//! Thin wrapper around the `qdrant-client` crate.
//!
//! Owns the client construction, config parsing, and error mapping. The
//! provisioner (see `provisioner.rs`) and worker (see `worker.rs`) call into
//! this module — they do not import `qdrant_client` directly.
//!
//! Per §6.11 dep graph: `notedthat-indexer` is the only crate that
//! depends on `qdrant-client`.

use qdrant_client::Qdrant;
use std::sync::Arc;

/// Qdrant client configuration, parsed from `NOTEDTHAT_QDRANT_*` env vars.
#[derive(Debug, Clone)]
pub struct QdrantConfig {
    /// Qdrant server URL (e.g., <http://127.0.0.1:6334>).
    pub url: String,
    /// Optional API key for authentication.
    pub api_key: Option<String>,
}

/// Errors from the Qdrant wrapper.
#[derive(Debug, thiserror::Error)]
pub enum QdrantWrapperError {
    /// Failed to build the Qdrant client.
    #[error("qdrant client build failed: {0}")]
    ClientBuild(String),
    /// Qdrant operation failed.
    #[error("qdrant operation failed: {0}")]
    Operation(String),
}

/// Thin wrapper: constructs a `Qdrant` client from config.
#[derive(Clone)]
pub struct QdrantClient {
    inner: Arc<Qdrant>,
}

impl QdrantClient {
    /// Build a `QdrantClient` from the provided config.
    ///
    /// Construction is cheap — no network connection is made until the first RPC call.
    pub fn new(config: &QdrantConfig) -> Result<Self, QdrantWrapperError> {
        let builder = Qdrant::from_url(&config.url);
        let builder = if let Some(key) = &config.api_key {
            builder.api_key(key.clone())
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|e| QdrantWrapperError::ClientBuild(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(client),
        })
    }

    /// Access the underlying `qdrant_client::Qdrant` for advanced operations.
    ///
    /// Kept `pub(crate)` so provisioner + worker can call directly, but the API
    /// does not leak out of this crate.
    pub(crate) fn inner(&self) -> &Qdrant {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_clone_debug() {
        let c = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: None,
        };
        let _c2 = c.clone();
        let _s = format!("{c:?}");
    }

    #[test]
    fn new_without_api_key() {
        let config = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: None,
        };
        // Construction should succeed (no network until first RPC)
        let result = QdrantClient::new(&config);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn new_with_api_key() {
        let config = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: Some("test-key".into()),
        };
        let result = QdrantClient::new(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn client_is_clone() {
        let config = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: None,
        };
        let client = QdrantClient::new(&config).unwrap();
        let _cloned = client.clone();
    }
}
