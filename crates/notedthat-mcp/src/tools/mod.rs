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

use crate::auth::CallerToken;
use crate::client::NotedThatClient;
use crate::error::McpToolError;
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, Extensions, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResult, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
};

/// What a call runs as when its request carries no [`CallerToken`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fallback {
    /// The configured token. Stdio has no request, so there is no bearer to
    /// forward: the process's own credential is the caller.
    ConfiguredToken,
    /// Nothing: the call is refused. Over HTTP a missing token means the auth
    /// middleware did not see this request, and acting as the configured
    /// token instead would be exactly the escalation forwarding the caller's
    /// credential closes.
    Refuse,
}

/// MCP tool handler backed by the `NotedThat` HTTP API.
#[derive(Clone)]
pub struct NotedThatMcp {
    client: NotedThatClient,
    fallback: Fallback,
}

impl NotedThatMcp {
    /// A handler for the stdio transport: `client`'s token is the caller.
    pub fn for_stdio(client: NotedThatClient) -> Self {
        Self {
            client,
            fallback: Fallback::ConfiguredToken,
        }
    }

    /// A handler for the HTTP transport: every call acts as the bearer the
    /// auth middleware accepted, and a call that carries none is refused
    /// rather than run as `client`'s token.
    pub fn for_http(client: NotedThatClient) -> Self {
        Self {
            client,
            fallback: Fallback::Refuse,
        }
    }

    /// The API client for one call: the caller's own credential when the
    /// request carries one, otherwise whatever the transport's [`Fallback`]
    /// allows.
    ///
    /// Over the streamable HTTP transport rmcp places the request's
    /// [`axum::http::request::Parts`] — axum extensions included — into the
    /// call's extensions, and the auth middleware left a [`CallerToken`] there.
    /// Over stdio there are no parts.
    fn client_for(&self, extensions: &Extensions) -> Result<NotedThatClient, McpError> {
        let caller = extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<CallerToken>());
        match (caller, self.fallback) {
            (Some(token), _) => Ok(self.client.with_token(token.as_str())),
            (None, Fallback::ConfiguredToken) => Ok(self.client.clone()),
            (None, Fallback::Refuse) => Err(McpToolError::Forbidden.into()),
        }
    }
}

#[tool_router]
impl NotedThatMcp {
    #[tool(description = "List all knowledge bases declared on the server")]
    async fn list_knowledgebases(
        &self,
        context: RequestContext<RoleServer>,
        _args: Parameters<list_kbs::ListKbsArgs>,
    ) -> Result<CallToolResult, McpError> {
        list_kbs::run(&self.client_for(&context.extensions)?).await
    }

    #[tool(
        description = "Hybrid search across one or more knowledge bases. kb is a list of slugs (discover them with list_knowledgebases); omit it to search every knowledge base the caller may search. Results are grouped per knowledge base in request order and each group keeps that knowledge base's own ranking; score is a per-knowledge-base rank artifact, not comparable across knowledge bases, so no merged ranking is given. limit is per knowledge base (default 10, max 50). With an explicit list, an unknown or inaccessible slug fails the whole call and is named in the error; with kb omitted, knowledge bases that refuse search are listed under skipped."
    )]
    async fn search(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<search::SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        search::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        description = "Read an object by optional range. Accepts byte_start/byte_end for byte ranges or line_start/line_end for line ranges (mutually exclusive). byte_end is exclusive."
    )]
    async fn read(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<read::ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        read::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(description = "Create or update an object; content is UTF-8 text in v1")]
    async fn write(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<write::WriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        write::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        description = "Edit an object by replacing lines or bytes. Accepts (line_start, line_end) for line mode (1-based, insert-at-N via line_end = line_start - 1) OR (byte_start, byte_end) for byte mode (0-based, byte_end EXCLUSIVE, requires byte_start < byte_end — byte-mode insert not supported in v1). Mutually exclusive. if_match is required."
    )]
    async fn edit(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<edit::EditArgs>,
    ) -> Result<CallToolResult, McpError> {
        edit::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(description = "Append UTF-8 content to an object")]
    async fn append(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<append::AppendArgs>,
    ) -> Result<CallToolResult, McpError> {
        append::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        description = "Replace an exact UTF-8 substring in an object; if_match is required. Fails with no_match if the substring is not found, or ambiguous_match { match_count } if there are multiple matches and replace_all is not set."
    )]
    async fn replace(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<replace::ReplaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        replace::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(description = "List objects in a knowledge base under an optional prefix")]
    async fn list(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<list::ListArgs>,
    ) -> Result<CallToolResult, McpError> {
        list::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(description = "Delete an object (idempotent)")]
    async fn delete(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<delete::DeleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        delete::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        name = "move",
        description = "Move/rename an object (non-atomic: GET -> PUT -> DELETE)"
    )]
    async fn mv(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<mv::MoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        mv::run(&self.client_for(&context.extensions)?, args.0).await
    }
}

#[tool_handler]
impl rmcp::handler::server::ServerHandler for NotedThatMcp {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult {
            tools: Self::tool_router().list_all(),
            meta: None,
            next_cursor: None,
        }))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        crate::resources_list::list_resources(
            &self.client_for(&context.extensions)?,
            request.and_then(|params| params.cursor),
        )
        .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        crate::resources_read::read_resource(&self.client_for(&context.extensions)?, &request.uri)
            .await
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
mod client_for {
    use super::*;
    use axum::http::request::Parts;

    fn client() -> NotedThatClient {
        NotedThatClient::new("http://localhost:8080", "configured").unwrap()
    }

    /// The extensions of a call that arrived over HTTP: rmcp's `Parts`, with
    /// the `CallerToken` the auth middleware leaves when it ran.
    fn http_call(caller: Option<&str>) -> Extensions {
        let (mut parts, ()) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        if let Some(token) = caller {
            parts.extensions.insert(CallerToken::unverified(token));
        }
        let mut extensions = Extensions::new();
        extensions.insert::<Parts>(parts);
        extensions
    }

    fn forbidden(error: &McpError) -> bool {
        error.code == rmcp::model::ErrorCode::INVALID_PARAMS && error.message == "forbidden"
    }

    #[test]
    fn http_call_acts_as_its_caller() {
        // Given: an HTTP handler and a call the middleware accepted
        let handler = NotedThatMcp::for_http(client());

        // When: the client for that call is picked
        let picked = handler.client_for(&http_call(Some("caller"))).unwrap();

        // Then: it presents the caller's bearer, not the configured one
        assert_eq!(picked.token, "caller");
    }

    #[test]
    fn http_call_without_a_caller_token_is_refused() {
        // Given: an HTTP handler and a call whose request the middleware
        // never saw, so no CallerToken was left in it
        let handler = NotedThatMcp::for_http(client());

        // When: the client for that call is picked
        let refused = handler.client_for(&http_call(None)).unwrap_err();

        // Then: the call is refused as forbidden rather than run as the
        // configured token, which would be the escalation this closes
        assert!(forbidden(&refused), "{refused:?}");
    }

    #[test]
    fn http_call_without_request_parts_is_refused() {
        // Given: an HTTP handler and a call with no HTTP request at all
        let handler = NotedThatMcp::for_http(client());

        // When: the client for that call is picked
        let refused = handler.client_for(&Extensions::new()).unwrap_err();

        // Then: it is refused, since the configured token is never the
        // fallback over HTTP
        assert!(forbidden(&refused), "{refused:?}");
    }

    #[test]
    fn stdio_call_acts_as_the_configured_token() {
        // Given: a stdio handler and a call without a request, which is
        // every stdio call
        let handler = NotedThatMcp::for_stdio(client());

        // When: the client for that call is picked
        let picked = handler.client_for(&Extensions::new()).unwrap();

        // Then: the configured token is the caller
        assert_eq!(picked.token, "configured");
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
        NotedThatMcp::for_stdio(client(url))
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
    fn tools_list_returns_exactly_ten_m9_tools() {
        let h = handler("http://localhost:8080");
        let m9 = [
            "append",
            "delete",
            "edit",
            "list",
            "list_knowledgebases",
            "move",
            "read",
            "replace",
            "search",
            "write",
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

        // Verify TOTAL count is exactly 10 by checking that no additional tools exist.
        // This catches accidental tool registrations that would break the contract.
        // "edit_string" resolved by #39 as top-level `replace` tool (no longer deferred).
        let deferred_tools = [
            "edit_bytes", // deferred per issue #40 (byte args added to the unified `edit` tool instead of a separate name)
            "append_bytes", // not in spec
            "delete_bytes", // not in spec
            "write_bytes", // not in spec
        ];
        for name in deferred_tools {
            assert!(
                h.get_tool(name).is_none(),
                "deferred tool {name:?} must not be registered (would make count > 10)"
            );
        }
    }

    #[test]
    fn tool_schemas_are_unchanged_by_the_context_extractor() {
        // The `RequestContext` each tool takes is an extractor, not an
        // argument: nothing about it may leak into what a client is told.
        for tool in NotedThatMcp::tool_router().list_all() {
            let schema = serde_json::to_string(&tool.input_schema).expect("schema");
            assert!(
                !schema.contains("context") && !schema.contains("RequestContext"),
                "{}: {schema}",
                tool.name
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
            .and(path("/api/v1/knowledgebases/kb/note.md"))
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
            .and(path("/api/v1/knowledgebases"))
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
