//! OpenAI-compatible HTTP embedder client per §6.4 / D18.

use crate::embedder::{Embedder, EmbedderError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Configuration for `OpenAiCompatibleEmbedder`.
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    /// Base URL of the OpenAI-compatible service, without `/v1/embeddings`.
    pub endpoint_url: String,
    /// Embedding model identifier to pass in the request body.
    pub model: String,
    /// Bearer token for the embedding endpoint.
    pub api_key: String,
    /// Expected embedding dimensionality.
    pub dim: usize,
    /// Approximate maximum input tokens accepted by the endpoint per input.
    pub max_input_tokens: usize,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Maximum number of total attempts for retriable failures.
    pub max_retries: u32,
}

/// HTTP client for OpenAI-compatible embedding endpoints.
pub struct OpenAiCompatibleEmbedder {
    client: reqwest::Client,
    config: OpenAiCompatibleConfig,
}

impl OpenAiCompatibleEmbedder {
    /// Build a new embedder from explicit configuration.
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, EmbedderError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| EmbedderError::Transport(e.to_string()))?;
        Ok(Self { client, config })
    }
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    #[allow(dead_code)]
    index: usize,
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for OpenAiCompatibleEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/v1/embeddings",
            self.config.endpoint_url.trim_end_matches('/')
        );
        let body = EmbeddingsRequest {
            model: &self.config.model,
            input: texts,
        };

        let mut attempts: u32 = 0;
        loop {
            attempts = attempts.saturating_add(1);
            let result = self
                .client
                .post(&url)
                .bearer_auth(&self.config.api_key)
                .json(&body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let parsed: EmbeddingsResponse = resp
                            .json()
                            .await
                            .map_err(|e| EmbedderError::Malformed(e.to_string()))?;
                        if parsed.data.len() != texts.len() {
                            return Err(EmbedderError::CountMismatch {
                                sent: texts.len(),
                                returned: parsed.data.len(),
                            });
                        }
                        if let Some(first) = parsed.data.first()
                            && first.embedding.len() != self.config.dim
                        {
                            return Err(EmbedderError::DimensionMismatch {
                                expected: self.config.dim,
                                actual: first.embedding.len(),
                            });
                        }
                        return Ok(parsed.data.into_iter().map(|d| d.embedding).collect());
                    }
                    let retriable = status.as_u16() == 429 || status.is_server_error();
                    let body_text = resp.text().await.unwrap_or_default();
                    if retriable && attempts < self.config.max_retries {
                        let backoff = compute_backoff(attempts);
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    if retriable {
                        return Err(EmbedderError::RetriesExhausted { attempts });
                    }
                    return Err(EmbedderError::Http {
                        status: status.as_u16(),
                        body: body_text,
                    });
                }
                Err(e) => {
                    if attempts < self.config.max_retries {
                        let backoff = compute_backoff(attempts);
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(EmbedderError::Transport(e.to_string()));
                }
            }
        }
    }

    fn dim(&self) -> usize {
        self.config.dim
    }
    fn max_input_tokens(&self) -> usize {
        self.config.max_input_tokens
    }
    fn model_id(&self) -> &str {
        &self.config.model
    }
}

/// Exponential backoff with deterministic jitter: 100ms * 2^(attempt-1), capped at 4s.
fn compute_backoff(attempt: u32) -> Duration {
    let base_ms: u64 = 100u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(6));
    let jitter_ms = u64::from(attempt).saturating_mul(17) % 50;
    Duration::from_millis(base_ms.saturating_add(jitter_ms).min(4_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_config(url: &str) -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig {
            endpoint_url: url.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dim: 3,
            max_input_tokens: 8192,
            timeout: Duration::from_secs(5),
            max_retries: 3,
        }
    }

    fn make_response(embeddings: Vec<Vec<f32>>) -> serde_json::Value {
        let data: Vec<serde_json::Value> = embeddings
            .into_iter()
            .enumerate()
            .map(|(i, emb)| serde_json::json!({ "index": i, "embedding": emb, "object": "embedding" }))
            .collect();
        serde_json::json!({ "object": "list", "data": data, "model": "test-model", "usage": { "prompt_tokens": 1, "total_tokens": 1 } })
    }

    #[tokio::test]
    async fn success_single_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("Authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(vec![vec![0.1, 0.2, 0.3]])),
            )
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let result = embedder.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(result, vec![vec![0.1_f32, 0.2_f32, 0.3_f32]]);
    }

    #[tokio::test]
    async fn success_two_inputs_in_order() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(vec![
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
            ])))
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let result = embedder
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], vec![1.0_f32, 0.0, 0.0]);
        assert_eq!(result[1], vec![0.0_f32, 1.0, 0.0]);
    }

    #[tokio::test]
    async fn empty_input_no_http_call() {
        let server = MockServer::start().await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let result = embedder.embed(&[]).await.unwrap();
        assert!(result.is_empty());
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 0, "should make zero HTTP calls for empty input");
    }

    #[tokio::test]
    async fn retry_on_429_then_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(vec![vec![1.0, 0.0, 0.0]])),
            )
            .mount(&server)
            .await;
        let mut config = make_config(&server.uri());
        config.timeout = Duration::from_secs(10);
        let embedder = OpenAiCompatibleEmbedder::new(config).unwrap();
        let result = embedder.embed(&["x".to_string()]).await.unwrap();
        assert_eq!(result, vec![vec![1.0_f32, 0.0, 0.0]]);
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            reqs.len(),
            3,
            "should make exactly 3 HTTP calls (2 retries + success)"
        );
    }

    #[tokio::test]
    async fn retry_on_500_then_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(vec![vec![0.0, 1.0, 0.0]])),
            )
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let result = embedder.embed(&["y".to_string()]).await.unwrap();
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn retries_exhausted_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let mut config = make_config(&server.uri());
        config.max_retries = 2;
        let embedder = OpenAiCompatibleEmbedder::new(config).unwrap();
        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(matches!(err, EmbedderError::RetriesExhausted { .. }));
    }

    #[tokio::test]
    async fn no_retry_on_400() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(matches!(err, EmbedderError::Http { status: 400, .. }));
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "400 must not be retried");
    }

    #[tokio::test]
    async fn no_retry_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(matches!(err, EmbedderError::Http { status: 401, .. }));
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
    }

    #[tokio::test]
    async fn count_mismatch_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(vec![vec![1.0, 0.0, 0.0]])),
            )
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let err = embedder
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            EmbedderError::CountMismatch {
                sent: 2,
                returned: 1
            }
        ));
    }

    #[tokio::test]
    async fn dimension_mismatch_error() {
        let server = MockServer::start().await;
        // return 2-dim vector but config expects 3
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(vec![vec![1.0, 0.0]])),
            )
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(matches!(
            err,
            EmbedderError::DimensionMismatch {
                expected: 3,
                actual: 2
            }
        ));
    }

    #[tokio::test]
    async fn malformed_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
            .mount(&server)
            .await;
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&server.uri())).unwrap();
        let err = embedder.embed(&["x".to_string()]).await.unwrap_err();
        assert!(matches!(err, EmbedderError::Malformed(_)));
    }

    #[tokio::test]
    async fn trailing_slash_url_works() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(vec![vec![1.0, 0.0, 0.0]])),
            )
            .mount(&server)
            .await;
        // Add trailing slash to URL
        let url = format!("{}/", server.uri());
        let embedder = OpenAiCompatibleEmbedder::new(make_config(&url)).unwrap();
        let result = embedder.embed(&["x".to_string()]).await.unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn backoff_is_monotonically_increasing() {
        let b1 = compute_backoff(1);
        let b2 = compute_backoff(2);
        let b3 = compute_backoff(3);
        assert!(b1 <= b2, "b1={b1:?} b2={b2:?}");
        assert!(b2 <= b3, "b2={b2:?} b3={b3:?}");
        assert!(b3 <= Duration::from_secs(4));
    }
}
