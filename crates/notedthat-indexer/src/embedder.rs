//! The `Embedder` trait — abstract external embedding endpoint.
//!
//! Per D18 / §6.4: external endpoints only. The concrete `OpenAiCompatibleEmbedder`
//! (see `embedder::openai`) implements this over HTTP with `reqwest`.

pub mod openai;

use async_trait::async_trait;

/// Errors that can occur when calling an embedding endpoint.
#[derive(Debug, thiserror::Error)]
pub enum EmbedderError {
    /// Network transport error (connection refused, DNS, TLS, etc.).
    #[error("embedder transport error: {0}")]
    Transport(String),
    /// Non-2xx HTTP status from the embedding endpoint.
    #[error("embedder returned HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body.
        body: String,
    },
    /// Response body could not be parsed as the expected shape.
    #[error("embedder returned malformed body: {0}")]
    Malformed(String),
    /// Number of embeddings returned did not match number of inputs sent.
    #[error("embedder returned {returned} embeddings for {sent} inputs")]
    CountMismatch {
        /// Number of inputs sent.
        sent: usize,
        /// Number of embeddings returned.
        returned: usize,
    },
    /// Embedding dimensions did not match the configured `EMBEDDING_DIMENSIONS`.
    #[error("embedder returned {actual}-dim vector but expected {expected}")]
    DimensionMismatch {
        /// Expected dimensionality.
        expected: usize,
        /// Actual dimensionality returned.
        actual: usize,
    },
    /// Retry budget exhausted (HTTP 429 or 5xx repeated).
    #[error("embedder retry budget exhausted after {attempts} attempts")]
    RetriesExhausted {
        /// Number of retry attempts made before giving up.
        attempts: u32,
    },
}

/// External embedding endpoint abstraction.
///
/// Spec §6.4 defines the exact signature. `embed` is batch-only; the caller
/// splits chunks into batches of `EMBEDDING_BATCH_SIZE` before calling.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts. Returns one embedding per input, in the same order as `texts`.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError>;

    /// Vector dimensionality of the embeddings this embedder produces.
    fn dim(&self) -> usize;

    /// Approximate maximum input tokens the endpoint accepts per input.
    fn max_input_tokens(&self) -> usize;

    /// Model identifier for observability (e.g., "text-embedding-3-small").
    fn model_id(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_transport() {
        let e = EmbedderError::Transport("connection refused".into());
        assert!(e.to_string().contains("transport error"));
    }

    #[test]
    fn error_display_http() {
        let e = EmbedderError::Http { status: 429, body: "rate limited".into() };
        assert!(e.to_string().contains("429"));
    }

    #[test]
    fn error_display_malformed() {
        let e = EmbedderError::Malformed("not json".into());
        assert!(e.to_string().contains("malformed"));
    }

    #[test]
    fn error_display_count_mismatch() {
        let e = EmbedderError::CountMismatch { sent: 2, returned: 1 };
        assert!(e.to_string().contains("2") && e.to_string().contains("1"));
    }

    #[test]
    fn error_display_dim_mismatch() {
        let e = EmbedderError::DimensionMismatch { expected: 1024, actual: 512 };
        assert!(e.to_string().contains("1024") && e.to_string().contains("512"));
    }

    #[test]
    fn error_display_retries_exhausted() {
        let e = EmbedderError::RetriesExhausted { attempts: 3 };
        assert!(e.to_string().contains("3"));
    }

    // Compile-only: EmbedderError is Send + Sync + 'static
    fn _error_bounds() where EmbedderError: Send + Sync + 'static {}

    // Compile-only: Embedder is object-safe
    fn _obj_safe<E: Embedder + ?Sized>() {}
}
