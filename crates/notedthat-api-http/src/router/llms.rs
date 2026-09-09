use axum::http::header::CONTENT_TYPE;

const NAVIGATION_INSTRUCTIONS: &str = r"# NotedThat API navigation

NotedThat stores Markdown documents in named knowledge bases.

Start by sending `GET /api/v1/knowledgebases` to list the knowledge bases available to the caller. Send `GET /api/v1/knowledgebases/{kb_slug}` to list objects in one knowledge base. Read an object with `GET /api/v1/knowledgebases/{kb_slug}/{path}`. Search with `POST /api/v1/knowledgebases/{kb_slug}/search` and a JSON search body.

Use `Authorization: Bearer <token>` for all `/api/v1/` requests unless the deployment has granted the matching verb to anonymous callers in that knowledge base's manifest: `list` for object listings, `read` for GET or HEAD object reads, and `search` for search. Omit the header only for a verb known to be granted; verbs are independent and can be scoped to part of a knowledge base, so search can return paths and previews for objects you cannot read, and a listing can show objects whose content is closed. A supplied malformed or invalid credential always returns `401 unauthorized`; it never falls back to anonymous access. `403 forbidden` means the credential is valid but the knowledge base does not grant this operation on this key — retrying with the same credential will not help. All writes require a credential the manifest grants `write` or `delete`.

A listing shows only what the caller may see, so a page can be shorter than the requested limit, or empty, while still reporting `truncated`. Drive pagination from `next_cursor` and never from the number of objects returned.

`/browse/` is a human-facing HTML view of the same content. Use `/api/v1/` instead; do not scrape it.

`/healthz`, `/readyz`, and this `/llms.txt` document are globally public. Read the API documentation for object-write, conditional-request, range-read, pagination, and search formats before using those operations. This document contains no credentials or deployment-specific data.
";

pub(super) async fn llms_txt() -> ([(axum::http::HeaderName, &'static str); 1], &'static str) {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        NAVIGATION_INSTRUCTIONS,
    )
}
