//! `notedthat-mcp`: MCP tool surface wrapping the `NotedThat` HTTP API.
//! Consumed by `notedthat-mcp-stdio` and any future MCP HTTP transport.

pub mod client;
pub mod error;
pub mod path;
/// MCP tool router and per-tool HTTP adapters.
pub mod tools;
pub use tools::NotedThatMcp;
