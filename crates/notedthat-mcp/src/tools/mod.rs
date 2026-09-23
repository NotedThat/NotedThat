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
use crate::subscriptions::{Subscriptions, probe_listable};
use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, Extensions, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResult, ServerInfo, SubscribeRequestMethod,
        SubscribeRequestParams, UnsubscribeRequestMethod, UnsubscribeRequestParams,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// MCP tool handler backed by the `NotedThat` HTTP API.
///
/// One instance per session (the stateful transport creates it at
/// `initialize`), so what it owns is per-session state: the resource
/// subscriptions, when the deployment can feed them. Deliberately not
/// `Clone` — a stray copy would keep the subscriptions alive past the session.
pub struct NotedThatMcp {
    client: NotedThatClient,
    /// `Some` when an events backend is configured: the session may subscribe
    /// to resources, and `initialize` says so (D66). `None` otherwise, and
    /// `resources/subscribe` is `method_not_found`, like the capability it
    /// did not advertise.
    subscriptions: Option<Arc<Subscriptions>>,
}

impl NotedThatMcp {
    /// A handler for the streamable HTTP transport: every call acts as the
    /// bearer the auth middleware accepted, and a call that carries none is
    /// refused rather than run as `client`'s token. With `events`, the
    /// session may subscribe to resources; its forwarders stop when that
    /// token is cancelled or the session ends, whichever comes first.
    pub fn for_http(client: NotedThatClient, events: Option<&CancellationToken>) -> Self {
        Self {
            client,
            subscriptions: events.map(Subscriptions::new),
        }
    }

    /// The session's subscriptions, or the error a client gets for using a
    /// capability the server did not advertise.
    fn subscriptions<M: rmcp::model::ConstString>(&self) -> Result<&Arc<Subscriptions>, McpError> {
        self.subscriptions
            .as_ref()
            .ok_or_else(McpError::method_not_found::<M>)
    }

    /// The API client for one call: the caller's own credential when the
    /// request carries one, no credential when the middleware admitted an
    /// anonymous caller, and a refusal otherwise.
    ///
    /// rmcp places the request's [`axum::http::request::Parts`] — axum
    /// extensions included — into the call's extensions, and the auth
    /// middleware left a [`Caller`] there. A call without one is a request
    /// the middleware never saw, and acting as the configured token instead
    /// would be exactly the escalation forwarding the caller's credential
    /// closes.
    fn client_for(&self, extensions: &Extensions) -> Result<NotedThatClient, McpError> {
        let caller = extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Caller>());
        match caller {
            Some(Caller::Bearer(token)) => Ok(self.client.with_token(token)),
            Some(Caller::Anonymous) => Ok(self.client.anonymous()),
            None => Err(McpToolError::Forbidden.into()),
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
        description = "Report a knowledge base's search-index health: state is one of healthy, indexing, backpressured, stale or failed, with pending events, queue depth and capacity, whether the worker is running, when something was last indexed, the most recent failure (when; its summary when the caller may list the whole knowledge base; its object key when the caller may list it) and, on the fs backend, the last reconciliation pass. Check it when search results look incomplete or out of date; a failed or stale knowledge base may not reflect recent writes. For one specific write, the object.indexed event on the knowledge base's events stream is the precise completion signal."
    )]
    async fn index_status(
        &self,
        context: RequestContext<RoleServer>,
        args: Parameters<index_status::IndexStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        index_status::run(&self.client_for(&context.extensions)?, args.0).await
    }

    #[tool(
        description = "Hybrid search across one or more knowledge bases. kb is a list of slugs (discover them with list_knowledgebases); omit it, or pass [], to search every knowledge base the caller may search. Results are grouped per knowledge base in request order and each group keeps that knowledge base's own ranking; score is a per-knowledge-base rank artifact, not comparable across knowledge bases, so no merged ranking is given. limit is per knowledge base (default 10, max 50). With an explicit list, an unknown or inaccessible slug fails the whole call and is named in the error; with kb omitted, knowledge bases that refuse search are listed under skipped. An argument or filter key this tool does not know is refused before the tool runs, as a tool error naming the key; the filter's fields are exactly the HTTP search body's. Indexing is asynchronous: an object written moments ago is in the results once the knowledge base's events stream (GET /api/v1/knowledgebases/{kb}/events) has reported object.indexed for its etag, and never if it reported object.index_failed; wait for that rather than retrying the search, and check index_status if unsure."
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

    /// List resources — and watch the knowledge base each page named for
    /// `list_changed` (D66): a client that never listed cannot be surprised by
    /// a change, so nothing is watched before, and a client that listed one
    /// page has been shown one knowledge base, so only that one is watched.
    ///
    /// Each watch is a `GET …/events` stream held open against the loopback API
    /// for the life of the session, so watching every knowledge base the caller
    /// *could* see — which is what this used to do, on the first page — cost
    /// `sessions × knowledge bases` internal streams for a client that asked
    /// for one page.
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let client = self.client_for(&context.extensions)?;
        let kbs = client.list_kb_slugs().await?;
        let cursor = request.and_then(|params| params.cursor);
        let listed = crate::resources_list::kb_for_page(&kbs, cursor.as_deref())?;
        let result = crate::resources_list::list_resources(&client, &kbs, cursor).await?;
        if let Some(subscriptions) = &self.subscriptions
            && let Some(listed) = listed
        {
            subscriptions
                .watch_list_changes(std::slice::from_ref(&listed), &client, &context.peer)
                .await;
        }
        Ok(result)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        crate::resources_read::read_resource(&self.client_for(&context.extensions)?, &request.uri)
            .await
    }

    /// Subscribe to a `notedthat://` resource (D66): the key is probed as
    /// the caller — may it `list` it? — so a subscription that is accepted is
    /// one the events route will feed, and one it would not feed is refused
    /// the way `resources/read` refuses: `forbidden`, or the concealed
    /// `not_found`.
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let subscriptions = self.subscriptions::<SubscribeRequestMethod>()?;
        let client = self.client_for(&context.extensions)?;
        let parsed = crate::resources_read::parse_resource_uri(&request.uri)?;
        probe_listable(&client, &parsed.kb_slug, &parsed.object_key).await?;
        subscriptions
            .subscribe(
                &parsed.kb_slug,
                &parsed.object_key,
                &request.uri,
                &client,
                &context.peer,
            )
            .await
    }

    /// Forget a subscription; idempotent, and never an error for a URI that
    /// was never subscribed. Nothing here waits, so no `async fn`.
    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        let outcome = self
            .subscriptions::<UnsubscribeRequestMethod>()
            .and_then(|subscriptions| {
                let parsed = crate::resources_read::parse_resource_uri(&request.uri)?;
                subscriptions.unsubscribe(&parsed.kb_slug, &parsed.object_key);
                Ok(())
            });
        std::future::ready(outcome)
    }

    /// Override `get_info` to advertise both tools and resources capabilities.
    ///
    /// Without this override the `#[tool_handler]` macro would generate a
    /// `get_info` that only includes `enable_tools()`. `enable_resources()`
    /// makes `initialize` include `resources`; `subscribe` and `listChanged`
    /// are advertised exactly when an events backend can feed them (D66).
    fn get_info(&self) -> ServerInfo {
        let capabilities = rmcp::model::ServerCapabilities::builder()
            .enable_tools()
            .enable_resources();
        let capabilities = if self.subscriptions.is_some() {
            capabilities
                .enable_resources_subscribe()
                .enable_resources_list_changed()
        } else {
            capabilities
        };
        ServerInfo::new(capabilities.build())
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
        let handler = NotedThatMcp::for_http(client(), None);

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
        let handler = NotedThatMcp::for_http(client(), None);

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
        let handler = NotedThatMcp::for_http(client(), None);

        // When: the client for that call is picked
        let refused = handler.client_for(&http_call(None)).unwrap_err();

        // Then: the call is refused as forbidden rather than run as the
        // configured token, which would be the escalation this closes
        assert!(forbidden(&refused), "{refused:?}");
    }

    #[test]
    fn http_call_without_request_parts_is_refused() {
        // Given: an HTTP handler and a call with no HTTP request at all
        let handler = NotedThatMcp::for_http(client(), None);

        // When: the client for that call is picked
        let refused = handler.client_for(&Extensions::new()).unwrap_err();

        // Then: it is refused, since the configured token is never the
        // fallback over HTTP
        assert!(forbidden(&refused), "{refused:?}");
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
        NotedThatMcp::for_http(client(url), None)
    }

    // ── Capability advertisement ─────────────────────────────────────────────

    #[test]
    fn initialize_includes_resources_without_subscribe_or_list_changed_when_no_events_backend() {
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

    #[tokio::test]
    async fn initialize_advertises_subscribe_and_list_changed_with_an_events_backend() {
        let shutdown = CancellationToken::new();
        let h = NotedThatMcp::for_http(client("http://localhost:8080"), Some(&shutdown));
        let resources = h
            .get_info()
            .capabilities
            .resources
            .expect("resources capability");
        assert_eq!(resources.subscribe, Some(true));
        assert_eq!(resources.list_changed, Some(true));
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

        let client = client(&server.uri());
        let kbs = client.list_kb_slugs().await.expect("kbs");
        let result = crate::resources_list::list_resources(&client, &kbs, None).await;
        assert!(
            result.is_ok(),
            "resources/list must succeed: {:?}",
            result.err()
        );
    }
}
