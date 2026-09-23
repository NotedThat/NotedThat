//! `notedthat-mcp`: MCP tool surface wrapping the `NotedThat` HTTP API.
//! Served over the streamable HTTP transport by `notedthat-server` at `POST /mcp`.

/// Caller authentication middleware for the MCP HTTP endpoint.
pub mod auth;
pub mod client;
pub mod error;
/// Streamable HTTP transport adapter for the MCP tool handler.
pub mod http;
pub mod path;
mod resources_list;
/// MCP `resources/read` implementation for `notedthat://` object URIs.
pub mod resources_read;
/// Incremental `text/event-stream` parsing, for the streams this crate reads.
pub mod sse;
pub mod sse_refusal;
mod subscriptions;
/// Test support: an MCP session client for the workspace's HTTP suites.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
/// MCP tool router and per-tool HTTP adapters.
pub mod tools;
pub use client::DEFAULT_MAX_READ_BYTES;
pub use http::{McpHttpService, McpHttpServiceConfig, McpHttpServiceConfigError};
pub use tools::NotedThatMcp;
