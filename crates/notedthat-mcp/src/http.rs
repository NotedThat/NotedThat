//! Stateless JSON-response Streamable HTTP transport for the MCP tool handler.

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{NotedThatMcp, client::NotedThatClient};

/// Axum/tower-compatible rmcp Streamable HTTP service type for `NotedThatMcp`.
pub type McpHttpInnerService = StreamableHttpService<NotedThatMcp, NeverSessionManager>;

/// Validated Streamable HTTP server configuration for stateless JSON responses.
#[derive(Clone, Debug)]
pub struct McpHttpServiceConfig {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    cancellation_token: CancellationToken,
}

impl McpHttpServiceConfig {
    /// Build validated HTTP transport config.
    pub fn new(
        allowed_hosts: impl IntoIterator<Item = impl Into<String>>,
        allowed_origins: impl IntoIterator<Item = impl Into<String>>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, McpHttpServiceConfigError> {
        let allowed_hosts = allowed_hosts
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let allowed_origins = allowed_origins
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();

        if allowed_hosts.is_empty() {
            return Err(McpHttpServiceConfigError::EmptyAllowedHosts);
        }
        if allowed_origins.is_empty() {
            return Err(McpHttpServiceConfigError::EmptyAllowedOrigins);
        }

        Ok(Self {
            allowed_hosts,
            allowed_origins,
            cancellation_token,
        })
    }

    fn streamable_http_config(&self) -> StreamableHttpServerConfig {
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(self.allowed_hosts.clone())
            .with_allowed_origins(self.allowed_origins.clone())
            .with_cancellation_token(self.cancellation_token.clone())
    }
}

/// Errors from constructing [`McpHttpServiceConfig`].
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum McpHttpServiceConfigError {
    /// Empty host lists disable rmcp Host validation and are not allowed here.
    #[error("allowed_hosts must not be empty")]
    EmptyAllowedHosts,
    /// Empty origin lists disable rmcp Origin validation and are not allowed here.
    #[error("allowed_origins must not be empty")]
    EmptyAllowedOrigins,
}

/// Reusable Streamable HTTP service wrapper for `NotedThatMcp`.
#[derive(Clone)]
pub struct McpHttpService {
    inner: McpHttpInnerService,
}

impl McpHttpService {
    /// Create a stateless JSON-response Streamable HTTP service.
    pub fn new(client: NotedThatClient, config: McpHttpServiceConfig) -> Self {
        let streamable_config = config.streamable_http_config();
        let service_factory = move || Ok(NotedThatMcp::new(client.clone()));
        let inner = StreamableHttpService::new(
            service_factory,
            Arc::new(NeverSessionManager::default()),
            streamable_config,
        );

        Self { inner }
    }

    /// Return the rmcp Streamable HTTP service for use with axum/tower routing.
    pub fn into_service(self) -> McpHttpInnerService {
        self.inner
    }

    /// Inspect the effective rmcp Streamable HTTP server config.
    pub fn config(&self) -> &StreamableHttpServerConfig {
        &self.inner.config
    }
}

#[cfg(test)]
mod mcp_http_service {
    use super::*;

    fn test_client() -> NotedThatClient {
        NotedThatClient::new("http://127.0.0.1:8080", "test-token")
            .expect("test client config is valid")
    }

    fn test_config(token: CancellationToken) -> McpHttpServiceConfig {
        McpHttpServiceConfig::new(["127.0.0.1", "localhost"], ["http://127.0.0.1:8080"], token)
            .expect("test HTTP service config is valid")
    }

    #[test]
    fn creates_service_with_configured_hosts_and_origins() {
        // Given: explicit non-empty host and origin allow-lists.
        let token = CancellationToken::new();
        let config = test_config(token);

        // When: the MCP HTTP service is created.
        let service = McpHttpService::new(test_client(), config);

        // Then: rmcp receives the caller-supplied allow-lists.
        assert_eq!(
            service.config().allowed_hosts,
            ["127.0.0.1".to_string(), "localhost".to_string()]
        );
        assert_eq!(
            service.config().allowed_origins,
            ["http://127.0.0.1:8080".to_string()]
        );
    }

    #[test]
    fn explicitly_sets_stateless_json_response_mode() {
        // Given: valid HTTP service config.
        let token = CancellationToken::new();
        let config = test_config(token);

        // When: the MCP HTTP service is created.
        let service = McpHttpService::new(test_client(), config);

        // Then: stateful mode is disabled and JSON responses are enabled.
        assert!(!service.config().stateful_mode);
        assert!(service.config().json_response);
    }

    #[test]
    fn uses_caller_supplied_cancellation_token() {
        // Given: a cancellation token owned by the caller.
        let token = CancellationToken::new();
        let config = test_config(token.clone());
        let service = McpHttpService::new(test_client(), config);

        // When: the caller cancels the original token.
        token.cancel();

        // Then: the rmcp config observes the same cancellation hook.
        assert!(service.config().cancellation_token.is_cancelled());
    }

    #[test]
    fn rejects_empty_host_or_origin_allow_lists() {
        // Given: rmcp treats empty allow-lists as validation disabled.
        let token = CancellationToken::new();

        // When/Then: empty host and origin lists are rejected before rmcp sees them.
        let empty_hosts =
            McpHttpServiceConfig::new(Vec::<&str>::new(), ["http://127.0.0.1:8080"], token.clone());
        assert!(matches!(
            empty_hosts,
            Err(McpHttpServiceConfigError::EmptyAllowedHosts)
        ));
        let empty_origins = McpHttpServiceConfig::new(["127.0.0.1"], Vec::<&str>::new(), token);
        assert!(matches!(
            empty_origins,
            Err(McpHttpServiceConfigError::EmptyAllowedOrigins)
        ));
    }

    #[test]
    fn mcp_http_service_is_send_sync() {
        const _: fn() = || {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<McpHttpService>();
        };
    }

    #[test]
    fn exposes_rmcp_service_for_axum_tower_routing() {
        // Given: a constructed wrapper.
        let token = CancellationToken::new();
        let config = test_config(token);
        let service = McpHttpService::new(test_client(), config);

        // When: callers request the underlying rmcp service.
        let inner = service.into_service();

        // Then: it preserves the required stateless JSON rmcp config.
        assert!(!inner.config.stateful_mode);
        assert!(inner.config.json_response);
    }
}
