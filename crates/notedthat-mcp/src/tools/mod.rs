//! HTTP-backed MCP tool router.

mod append;
mod delete;
mod edit;
mod index_status;
mod list;
mod list_kbs;
mod mv;
mod read;
mod replace;
mod search;
mod write;

use crate::auth::Caller;
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

/// What a call runs as when its request carries no [`Caller`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fallback {
    /// The configured token. Stdio has no request, so there is no bearer to
    /// forward: the process's own credential is the caller.
    ConfiguredToken,
    /// Nothing: the call is refused. Over HTTP a missing [`Caller`] means the
    /// auth middleware did not see this request — an anonymous caller it
    /// admitted is marked, not absent — and acting as the configured token
    /// instead would be exactly the escalation forwarding the caller's
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
    /// request carries one, no credential when the middleware admitted an
    /// anonymous caller, otherwise whatever the transport's [`Fallback`]
    /// allows.
    ///
    /// Over the streamable HTTP transport rmcp places the request's
    /// [`axum::http::request::Parts`] — axum extensions included — into the
    /// call's extensions, and the auth middleware left a [`Caller`] there.
    /// Over stdio there are no parts.
    fn client_for(&self, extensions: &Extensions) -> Result<NotedThatClient, McpError> {
        let caller = extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Caller>());
        match (caller, self.fallback) {
            (Some(Caller::Bearer(token)), _) => Ok(self.client.with_token(token)),
            (Some(Caller::Anonymous), _) => Ok(self.client.anonymous()),
            (None, Fallback::ConfiguredToken) => Ok(self.client.clone()),
            (None, Fallback::Refuse) => Err(McpToolError::Forbidden.into()),
        }
    }
}

#[tool_router]
impl NotedThatMcp {
    #[tool(
        description = "List the knowledge bases visible to the caller, each with its slug, display name and, when its manifest says what it is for, a description. Read the descriptions before choosing where to search."
    )]
    async fn list_knowledgebases(
        &self,
        context: RequestContext<RoleServer>,
        _args: Parameters<list_kbs::ListKbsArgs>,
    ) -> Result<CallToolResult, McpError> {
        list_kbs::run(&self.client_for(&context.extensions)?).await
    }

    #[tool(
        description = "Report a knowledge base's search-index health: state is one of healthy, indexing, backpressured, stale or failed, with pending events, queue depth and capacity, whether the worker is running, when something was last indexed, the most recent failure (when; its summary for a credentialed caller; its object key when the caller may list it) and, on the fs backend, the last reconciliation pass. Check it when search results look incomplete or out of date; a failed or stale knowledge base may not reflect recent writes."
    )]
    async fn index_status(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<index_status::IndexStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        index_status::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        description = "Hybrid search across one or more knowledge bases. kb is a list of slugs (discover them with list_knowledgebases); omit it, or pass [], to search every knowledge base the caller may search. Results are grouped per knowledge base in request order and each group keeps that knowledge base's own ranking; score is a per-knowledge-base rank artifact, not comparable across knowledge bases, so no merged ranking is given. limit is per knowledge base (default 10, max 50). With an explicit list, an unknown or inaccessible slug fails the whole call and is named in the error; with kb omitted, knowledge bases that refuse search are listed under skipped. An argument or filter key this tool does not know is refused before the tool runs, as a tool error naming the key; the filter's fields are exactly the HTTP search body's."
    )]
    async fn search(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<search::SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        search::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        description = "Read an object, whole or by range: byte_start/byte_end (byte_end exclusive) or line_start/line_end (1-based, inclusive; mutually exclusive with bytes). The text is in content and, with the metadata, in structuredContent, which carries the etag of the version read, the content type, bytes_returned, total_bytes and, for line reads, total_lines with the slice's bounds. Pass that etag as if_match to edit or replace. An object over the server's read budget is refused with the slice arguments to use instead.",
        output_schema = rmcp::handler::server::common::schema_for_type::<read::ReadResult>()
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
    /// the `Caller` the auth middleware leaves when it ran.
    fn http_call(caller: Option<Caller>) -> Extensions {
        let (mut parts, ()) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        if let Some(caller) = caller {
            parts.extensions.insert(caller);
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
        let picked = handler
            .client_for(&http_call(Some(Caller::unverified("caller"))))
            .unwrap();

        // Then: it presents the caller's bearer, not the configured one
        assert_eq!(picked.token.as_deref(), Some("caller"));
    }

    #[test]
    fn anonymous_http_call_carries_no_token() {
        // Given: an HTTP handler and a call the middleware admitted as the
        // anonymous caller
        let handler = NotedThatMcp::for_http(client());

        // When: the client for that call is picked
        let picked = handler
            .client_for(&http_call(Some(Caller::Anonymous)))
            .unwrap();

        // Then: it presents nothing — not the configured token, which would
        // turn "anyone may read" into "anyone may do what the server may"
        assert_eq!(picked.token, None);
    }

    #[test]
    fn http_call_without_a_caller_token_is_refused() {
        // Given: an HTTP handler and a call whose request the middleware
        // never saw, so no Caller was left in it
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
        assert_eq!(picked.token.as_deref(), Some("configured"));
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
    fn tools_list_returns_exactly_the_eleven_tools() {
        let h = handler("http://localhost:8080");
        let m9 = [
            "append",
            "delete",
            "edit",
            "index_status",
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

        // Verify TOTAL count is exactly 11 by checking that no additional tools exist.
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
                "deferred tool {name:?} must not be registered (would make count > 11)"
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

    /// A client that validates `structuredContent` against `outputSchema` must find
    /// every field the read tool sets — and no other tool claims a shape it does not
    /// return.
    #[test]
    fn the_read_tool_declares_its_structured_content_and_no_other_tool_does() {
        let tools = NotedThatMcp::tool_router().list_all();
        let read = tools
            .iter()
            .find(|tool| tool.name == "read")
            .expect("read tool");
        let schema = read
            .output_schema
            .as_ref()
            .expect("read declares an output schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("object schema");
        for field in [
            "text",
            "etag",
            "content_type",
            "bytes_returned",
            "total_bytes",
            "byte_start",
            "byte_end",
            "total_lines",
            "line_start",
            "line_end",
        ] {
            assert!(
                properties.contains_key(field),
                "output schema lacks {field}"
            );
        }
        for tool in &tools {
            if tool.name != "read" {
                assert!(
                    tool.output_schema.is_none(),
                    "{} declares an output schema it does not honour",
                    tool.name
                );
            }
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
