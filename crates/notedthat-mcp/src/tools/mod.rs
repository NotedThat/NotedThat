//! HTTP-backed MCP tool router.

mod delete;
mod list;
mod list_kbs;
mod mv;
mod read;
mod search;
mod write;

use crate::client::NotedThatClient;
use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::*, tool, tool_handler,
    tool_router,
};

/// MCP tool handler backed by the NotedThat HTTP API.
#[derive(Clone)]
pub struct NotedThatMcp {
    client: NotedThatClient,
}

impl NotedThatMcp {
    /// Create a new tool handler with the given HTTP client.
    pub fn new(client: NotedThatClient) -> Self {
        Self { client }
    }
}

#[tool_router]
impl NotedThatMcp {
    #[tool(description = "List all knowledge bases declared on the server")]
    async fn list_knowledgebases(
        &self,
        _args: Parameters<list_kbs::ListKbsArgs>,
    ) -> Result<CallToolResult, McpError> {
        list_kbs::run(&self.client).await
    }

    #[tool(description = "Hybrid search across a knowledge base")]
    async fn search(
        &self,
        args: Parameters<search::SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        search::run(&self.client, args.0).await
    }

    #[tool(description = "Read an object by byte range (optional); byte_end is exclusive")]
    async fn read(&self, args: Parameters<read::ReadArgs>) -> Result<CallToolResult, McpError> {
        read::run(&self.client, args.0).await
    }

    #[tool(description = "Create or update an object; content is UTF-8 text in v1")]
    async fn write(&self, args: Parameters<write::WriteArgs>) -> Result<CallToolResult, McpError> {
        write::run(&self.client, args.0).await
    }

    #[tool(description = "List objects in a knowledge base under an optional prefix")]
    async fn list(&self, args: Parameters<list::ListArgs>) -> Result<CallToolResult, McpError> {
        list::run(&self.client, args.0).await
    }

    #[tool(description = "Delete an object (idempotent)")]
    async fn delete(
        &self,
        args: Parameters<delete::DeleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        delete::run(&self.client, args.0).await
    }

    #[tool(
        name = "move",
        description = "Move/rename an object (non-atomic: GET -> PUT -> DELETE)"
    )]
    async fn mv(&self, args: Parameters<mv::MoveArgs>) -> Result<CallToolResult, McpError> {
        mv::run(&self.client, args.0).await
    }
}

#[tool_handler]
impl rmcp::handler::server::ServerHandler for NotedThatMcp {}
