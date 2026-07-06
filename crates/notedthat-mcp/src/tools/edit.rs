use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditArgs {
    /// Knowledge base slug.
    pub kb: String,
    /// Object path within the knowledge base.
    pub path: String,
    /// First line to replace (1-based inclusive).
    pub line_start: u64,
    /// Last line to replace (1-based inclusive). Set to `line_start - 1` for an insert point.
    pub line_end: u64,
    /// Replacement content.
    pub content: String,
    /// Required `ETag` from a previous GET or write (concurrency control).
    pub if_match: String,
}

#[derive(Debug, Serialize)]
struct EditResult {
    etag: Option<String>,
    location: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: EditArgs,
) -> Result<CallToolResult, McpError> {
    if args.line_start < 1 {
        return Err(
            McpToolError::InvalidRequest("line_start must be >= 1 (1-based)".into()).into(),
        );
    }
    if args.line_start > args.line_end.saturating_add(1) {
        return Err(McpToolError::InvalidRequest(
            "line_start must be <= line_end + 1 (set line_end = line_start - 1 for insert)".into(),
        )
        .into());
    }

    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let content_range = format!("lines {}-{}/*", args.line_start, args.line_end);
    let req = client
        .authorized(client.http.patch(url))
        .header("Content-Range", content_range)
        .header("If-Match", &args.if_match)
        .body(args.content.into_bytes());

    let resp = req.send().await.map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;

    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let result = EditResult { etag, location };
    Ok(CallToolResult::success(vec![ContentBlock::json(result)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    fn edit_args(line_start: u64, line_end: u64) -> EditArgs {
        EditArgs {
            kb: "notes".into(),
            path: "hello.md".into(),
            line_start,
            line_end,
            content: "replacement".into(),
            if_match: "\"abc\"".into(),
        }
    }

    #[tokio::test]
    async fn sends_patch_with_line_range_and_if_match_headers() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "lines 2-3/*"))
            .and(header("If-Match", "\"abc\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(2, 3)).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn ok_response_returns_etag() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"next\"")
                    .insert_header("Location", "/v1/knowledgebases/notes/hello.md"),
            )
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(1, 1)).await.unwrap();
        let rendered = format!("{result:?}");

        assert!(rendered.contains("next"), "etag missing: {rendered}");
    }

    #[tokio::test]
    async fn precondition_failed_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error": "precondition_failed",
                "message": "etag mismatch"
            })))
            .mount(&server)
            .await;

        let c = client(&server.uri());

        assert!(run(&c, edit_args(1, 1)).await.is_err());
    }

    #[tokio::test]
    async fn line_start_zero_returns_invalid_request_without_http_call() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(0, 0)).await;

        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn reverse_range_that_is_not_insert_returns_invalid_request_without_http_call() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(5, 3)).await;

        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn insert_point_encoding_sends_reversed_adjacent_line_range() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "lines 5-4/*"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(5, 4)).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }
}
