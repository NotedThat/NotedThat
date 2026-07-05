//! Pending M8 MCP HTTP transport and Resources contract tests.

#[test]
#[ignore = "pending M8 implementation: Bearer auth must reject missing or wrong tokens with 401"]
fn mcp_http_auth() {
    // Given: the future Streamable HTTP MCP endpoint at POST /mcp.
    // When: the Authorization header is missing or has the wrong Bearer token.
    // Then: the response contract is HTTP 401 for both cases.
    todo!("lock POST /mcp Bearer auth: missing header -> 401; wrong token -> 401");
}

#[test]
#[ignore = "pending M8 implementation: legacy SSE endpoints must return exact 405 JSON refusal"]
fn sse_refusal() {
    // Given: legacy SSE-shaped requests GET /mcp, DELETE /mcp, and POST /sse.
    // When: the server receives any of those requests.
    // Then: it returns HTTP 405 with exactly:
    // {"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}
    todo!("lock exact JSON refusal body for GET/DELETE /mcp and POST /sse");
}

#[test]
#[ignore = "pending M8 implementation: resources/list must flatten resources across KBs and paginate with cursor"]
fn resources_list() {
    // Given: multiple configured knowledge bases with objects under each KB.
    // When: an MCP client calls resources/list with and without PaginatedRequestParams.cursor.
    // Then: resources are returned flat across KBs as notedthat://<kb_slug>/<percent-encoded object_key>, with next_cursor when more remain.
    todo!("lock resources/list flat cross-KB listing and cursor contract");
}

#[test]
#[ignore = "pending M8 implementation: resources/read must cover text and blob content branches"]
fn resources_read() {
    // Given: notedthat:// resource URIs for UTF-8 text objects and binary/blob objects.
    // When: an MCP client calls resources/read for each branch.
    // Then: text objects return text contents and blob objects return blob contents according to the MCP resource contract.
    todo!("lock resources/read text and blob branches");
}

#[test]
#[ignore = "pending M8 implementation: resources and tools must not expose annotations or output_schema"]
fn no_resource_annotations() {
    // Given: tools/list and resources/list responses from the MCP server.
    // When: a client inspects the advertised tool and resource metadata.
    // Then: resources have no annotations/title/description, tools have no output_schema, and resource names equal object_key.
    todo!("lock absence of resource annotations and tool output_schema");
}
