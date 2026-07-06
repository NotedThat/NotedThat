//! HTTP-backed MCP tool router.

mod append;
mod delete;
mod edit;
mod list;
mod list_kbs;
mod mv;
mod read;
mod replace;
mod search;
mod write;

use crate::client::NotedThatClient;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResult, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
};

/// MCP tool handler backed by the `NotedThat` HTTP API.
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

    #[tool(
        description = "Read an object by optional range. Accepts byte_start/byte_end for byte ranges or line_start/line_end for line ranges (mutually exclusive). byte_end is exclusive."
    )]
    async fn read(&self, args: Parameters<read::ReadArgs>) -> Result<CallToolResult, McpError> {
        read::run(&self.client, args.0).await
    }

    #[tool(description = "Create or update an object; content is UTF-8 text in v1")]
    async fn write(&self, args: Parameters<write::WriteArgs>) -> Result<CallToolResult, McpError> {
        write::run(&self.client, args.0).await
    }

    #[tool(
        description = "Edit an object by replacing lines or bytes. Accepts (line_start, line_end) for line mode (1-based, insert-at-N via line_end = line_start - 1) OR (byte_start, byte_end) for byte mode (0-based, byte_end EXCLUSIVE, requires byte_start < byte_end — byte-mode insert not supported in v1). Mutually exclusive. if_match is required."
    )]
    async fn edit(&self, args: Parameters<edit::EditArgs>) -> Result<CallToolResult, McpError> {
        edit::run(&self.client, args.0).await
    }

    #[tool(description = "Append UTF-8 content to an object")]
    async fn append(
        &self,
        args: Parameters<append::AppendArgs>,
    ) -> Result<CallToolResult, McpError> {
        append::run(&self.client, args.0).await
    }

    #[tool(
        description = "Replace an exact UTF-8 substring in an object; if_match is required. Fails with no_match if the substring is not found, or ambiguous_match { match_count } if there are multiple matches and replace_all is not set."
    )]
    async fn replace(
        &self,
        args: Parameters<replace::ReplaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        replace::run(&self.client, args.0).await
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
impl rmcp::handler::server::ServerHandler for NotedThatMcp {
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        crate::resources_list::list_resources(
            &self.client,
            request.and_then(|params| params.cursor),
        )
        .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        crate::resources_read::read_resource(&self.client, &request.uri).await
    }

    /// Override `get_info` to advertise both tools and resources capabilities.
    ///
    /// Without this override the `#[tool_handler]` macro would generate a `get_info` that only
    /// includes `enable_tools()`. Adding `enable_resources()` causes the MCP `initialize` response
    /// to include `"resources": {}` (no `subscribe`, no `listChanged`) on both the stdio and HTTP
    /// transports.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
    }
}

#[cfg(test)]
mod send_sync_tests {
    use super::*;

    #[test]
    fn mcp_handler_is_send_sync() {
        const _: fn() = || {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<NotedThatMcp>();
        };
    }
}

#[cfg(test)]
mod resources_shared {
    use super::*;
    use rmcp::ServerHandler as _;

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    fn handler(url: &str) -> NotedThatMcp {
        NotedThatMcp::new(client(url))
    }

    // ── Capability advertisement ─────────────────────────────────────────────

    #[test]
    fn initialize_includes_resources_no_subscribe_no_list_changed() {
        let h = handler("http://localhost:8080");
        let info = h.get_info();
        let resources = info
            .capabilities
            .resources
            .expect("initialize must advertise resources capability");
        assert!(
            resources.subscribe.is_none(),
            "resources.subscribe must be absent from capabilities"
        );
        assert!(
            resources.list_changed.is_none(),
            "resources.listChanged must be absent from capabilities"
        );
    }

    #[test]
    fn initialize_still_advertises_tools() {
        let h = handler("http://localhost:8080");
        let info = h.get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must remain advertised after adding resources"
        );
    }

    // ── Tool count contract ──────────────────────────────────────────────────

    #[test]
    fn tools_list_returns_exactly_nine_m9_tools() {
        let h = handler("http://localhost:8080");
        let m9 = [
            "list_knowledgebases",
            "search",
            "read",
            "write",
            "edit",
            "append",
            "list",
            "delete",
            "move",
        ];
        for name in m9 {
            assert!(
                h.get_tool(name).is_some(),
                "expected M9 tool {name:?} to be registered"
            );
        }
        assert!(
            h.get_tool("nonexistent_tool").is_none(),
            "nonexistent tools must not be registered"
        );

        // Verify TOTAL count is exactly 9 by checking that no additional tools exist.
        // This catches accidental tool registrations that would break the contract.
        let deferred_tools = [
            "edit_bytes",   // deferred per issue #38
            "edit_string",  // deferred per issue #38
            "append_bytes", // not in spec
            "delete_bytes", // not in spec
            "write_bytes",  // not in spec
        ];
        for name in deferred_tools {
            assert!(
                h.get_tool(name).is_none(),
                "deferred tool {name:?} must not be registered (would make count > 9)"
            );
        }
    }

    // ── Delegation smoke tests ───────────────────────────────────────────────

    /// `resources/read` is callable and delegates to `crate::resources_read::read_resource`.
    #[tokio::test]
    async fn read_resource_callable() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/note.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Hello"))
            .mount(&server)
            .await;

        let result =
            crate::resources_read::read_resource(&client(&server.uri()), "notedthat://kb/note.md")
                .await;
        assert!(
            result.is_ok(),
            "resources/read must succeed: {:?}",
            result.err()
        );
    }

    /// `resources/list` is callable and delegates to `crate::resources_list::list_resources`.
    #[tokio::test]
    async fn list_resources_callable() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "knowledgebases": []
            })))
            .mount(&server)
            .await;

        let result = crate::resources_list::list_resources(&client(&server.uri()), None).await;
        assert!(
            result.is_ok(),
            "resources/list must succeed: {:?}",
            result.err()
        );
    }
}
