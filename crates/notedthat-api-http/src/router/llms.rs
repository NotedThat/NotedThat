use axum::http::header::CONTENT_TYPE;

const NAVIGATION_INSTRUCTIONS: &str = r#"# NotedThat navigation

NotedThat stores Markdown documents in named knowledge bases and serves them three ways on this same host: an HTTP API under `/api/v1/`, an MCP server at `POST /mcp`, and a WebDAV share under `/webdav/`. One set of access rules per knowledge base governs all three, so whatever a caller may do on one surface it may do on the others.

## Access rules, on every surface

A caller is either signed in — it presented `Authorization: Bearer <token>`, the deployment's service token or an access token its identity provider issued — or anonymous, having presented nothing. Each knowledge base grants verbs (`list`, `read`, `write`, `delete`, `search`) to signed-in callers, to named groups or users, or to `anyone`, optionally scoped to part of the knowledge base. Omit the credential only for a verb known to be granted to `anyone`. Verbs are independent, so search can return paths and previews for objects you cannot read, and a listing can show objects whose content is closed. A supplied malformed or invalid credential always returns `401 unauthorized`; it never falls back to anonymous access. `403 forbidden` means the credential is valid but the knowledge base does not grant this operation on this key — retrying with the same credential will not help. An anonymous caller that is not granted something gets `404 not_found`, indistinguishable from a knowledge base that does not exist. All writes require a credential the manifest grants `write` or `delete`.

## HTTP API

Start by sending `GET /api/v1/knowledgebases` to list the knowledge bases available to the caller. Send `GET /api/v1/knowledgebases/{kb_slug}` to list objects in one knowledge base. Read an object with `GET /api/v1/knowledgebases/{kb_slug}/{path}`. Search with `POST /api/v1/knowledgebases/{kb_slug}/search` and a JSON search body.

The search body is `{"query": "...", "filter": {...}, "limit": 10}`. `query` is required, 1–8192 bytes. `filter` is optional and its fields are AND-composed: `object_key_prefix` (hits whose key starts with this), `mime` (exactly this MIME type, e.g. `text/markdown`), `heading_path_prefix` (an array of heading segments the hit's heading path must start with), `updated_after` and `updated_before` (Unix seconds), and for Open Knowledge Format concepts `concept_type` (exact) and `tags` (any of). `limit` defaults to 10 and is capped at 50. An unknown key at either level — `filters` for `filter`, say — is `400 invalid_request` naming the key; nothing is silently ignored. Each hit carries `object_key`, `byte_start`, `byte_end`, `heading_path`, `preview` and `score`. `score` is a reciprocal-rank-fusion value: higher is better within one response, but it is not a probability or a similarity, is not comparable across queries or knowledge bases, and should not be shown as confidence. A non-empty knowledge base returns up to `limit` hits for any query, however unrelated, so an empty `hits` array is not how "the corpus does not cover this" announces itself — judge relevance from the previews.

A listing shows only what the caller may see, so a page can be shorter than the requested limit, or empty, while still reporting `truncated`. Drive pagination from `next_cursor` and never from the number of objects returned.

`GET /api/v1/knowledgebases/{kb_slug}/events` streams object change events as `text/event-stream` to callers the knowledge base grants `list`; send `Last-Event-ID` to resume after a disconnect, expect `410 gone` when that position is no longer retained, and `404 not_found` when the deployment has no events backend. Prefer it to polling listings when reacting to changes.

`/browse/` is a human-facing HTML view of the same content. Use `/api/v1/` instead; do not scrape it.

## MCP

`POST /mcp` is a Model Context Protocol server over the streamable HTTP transport in stateless JSON-response mode: every request is one complete JSON-RPC exchange answered with JSON, and no session is kept between requests. `GET /mcp`, `DELETE /mcp` and the legacy `/sse` paths answer `405`. It offers ten tools — `list_knowledgebases`, `search`, `read`, `list`, `write`, `edit`, `append`, `replace`, `move`, `delete` — and `notedthat://` resources. `search` takes `kb` (an array of slugs; omit it to search every knowledge base the caller may see), `query`, `filters` — the same seven fields as the HTTP body's `filter` — and a per-knowledge-base `limit`. Every tool call runs as the caller, bound by the access rules above; a denial surfaces as a tool error (`forbidden` for a signed-in caller, `not_found` for an anonymous one), never as a partial result.

Send the credential as `Authorization: Bearer <token>` on every request. Where the deployment grants `anyone` some verb, the header may be omitted: `initialize` and `tools/list` then succeed, `list_knowledgebases` names only the knowledge bases anonymous callers may see, and the read-only tools work where `anyone` holds the matching verb. `write`, `edit`, `append`, `replace`, `move` and `delete` always need a credential. A deployment that grants anonymous callers nothing, or that has anonymous MCP switched off, answers a missing credential with `401`; its `WWW-Authenticate` header names the authorization server when the deployment has one, which is how an OAuth-capable MCP client signs in.

A client that speaks the streamable HTTP transport needs only this configuration, with the host filled in and the `headers` entry dropped for anonymous use:

```json
{
  "mcpServers": {
    "notedthat": {
      "type": "http",
      "url": "https://HOST/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

## WebDAV

`/webdav/` serves the same knowledge bases as a WebDAV share, one directory per knowledge base. It accepts `Authorization: Bearer <token>` like the other surfaces and, alone among them, HTTP Basic with the deployment's WebDAV username and password. The access rules apply per method: `PROPFIND` needs `list`, `GET` and `HEAD` need `read`, `PUT`, `MKCOL` and `COPY` need `write`, `DELETE` needs `delete`, and `MOVE` needs both; where `anyone` holds a verb the corresponding methods work without credentials. It is a Class 1 server: there is no `LOCK`, so clients that insist on one before saving cannot write through it.

## Everything else

`/healthz`, `/readyz`, and this `/llms.txt` document are globally public. Read the API documentation for object-write, conditional-request, range-read, pagination, and search formats before using those operations. This document contains no credentials or deployment-specific data.
"#;

pub(super) async fn llms_txt() -> ([(axum::http::HeaderName, &'static str); 1], &'static str) {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        NAVIGATION_INSTRUCTIONS,
    )
}
