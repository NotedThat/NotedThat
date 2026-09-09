use axum::http::header::CONTENT_TYPE;

const NAVIGATION_INSTRUCTIONS: &str = r"# NotedThat API navigation

NotedThat stores Markdown documents in named knowledge bases.

Start by sending `GET /api/v1/knowledgebases` to list the knowledge bases available to the caller. Send `GET /api/v1/knowledgebases/{kb_slug}` to list objects in one knowledge base. Read an object with `GET /api/v1/knowledgebases/{kb_slug}/{path}`. Search with `POST /api/v1/knowledgebases/{kb_slug}/search` and a JSON search body.

Use `Authorization: Bearer <token>` for all `/api/v1/` requests unless the deployment has explicitly enabled the matching anonymous capability in that knowledge base's manifest: `discover` for the knowledge-base list, `browse` for object listings, `content` for GET or HEAD object reads, and `search` for search. Omit the header only for a capability known to be enabled; capabilities are independent, so search can expose paths and snippets without granting browsing or content reads. A supplied malformed or invalid credential always returns `401 unauthorized`; it never falls back to anonymous access. A valid Bearer token has full access to every declared knowledge base. All writes require a valid Bearer token.

`/healthz`, `/readyz`, and this `/llms.txt` document are globally public. Read the API documentation for object-write, conditional-request, range-read, pagination, and search formats before using those operations. This document contains no credentials or deployment-specific data.
";

pub(super) async fn llms_txt() -> ([(axum::http::HeaderName, &'static str); 1], &'static str) {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        NAVIGATION_INSTRUCTIONS,
    )
}
