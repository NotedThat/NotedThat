use axum::http::header::CONTENT_TYPE;

const NAVIGATION_INSTRUCTIONS: &str = r"# NotedThat API navigation

NotedThat stores Markdown documents in named knowledge bases.

Start by sending `GET /v1/knowledgebases` to list the knowledge bases available to the caller. Send `GET /v1/knowledgebases/{kb_slug}` to list objects in one knowledge base. Read an object with `GET /v1/knowledgebases/{kb_slug}/{path}`. Search with `POST /v1/knowledgebases/{kb_slug}/search` and a JSON search body.

All `/v1/` requests require an `Authorization: Bearer <token>` header. Read the API documentation for the object-write, conditional-request, range-read, pagination, and search request formats before using those operations. This `/llms.txt` document is public and contains no credentials or deployment-specific data.
";

pub(super) async fn llms_txt() -> ([(axum::http::HeaderName, &'static str); 1], &'static str) {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        NAVIGATION_INSTRUCTIONS,
    )
}
