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
pub struct AppendArgs {
    /// Knowledge base slug.
    pub kb: String,
    /// Object path within the knowledge base.
    pub path: String,
    /// Content to append to the end of the object.
    pub content: String,
    /// Optional ETag for optimistic concurrency. When omitted, the server
    /// obtains the current ETag internally — this is a true single round-trip.
    pub if_match: Option<String>,
}

#[derive(Debug, Serialize)]
struct AppendResult {
    etag: Option<String>,
    location: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: AppendArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let mut req = client
        .authorized(client.http.patch(url))
        .header("NT-Patch-Mode", "append")
        .body(args.content.into_bytes());

    if let Some(ref v) = args.if_match {
        req = req.header("If-Match", v);
    }

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

    let result = AppendResult { etag, location };
    Ok(CallToolResult::success(vec![ContentBlock::json(result)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{header, header_exists, method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    fn append_args(if_match: Option<String>) -> AppendArgs {
        AppendArgs {
            kb: "notes".into(),
            path: "hello.md".into(),
            content: "\nappended".into(),
            if_match,
        }
    }

    #[tokio::test]
    async fn sends_single_patch_with_append_mode_and_if_match_when_supplied() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header("nt-patch-mode", "append"))
            .and(header("if-match", "\"abc\""))
            .and(|req: &Request| !req.headers.contains_key("content-range"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, append_args(Some("\"abc\"".into()))).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sends_single_patch_with_append_mode_and_no_if_match_when_omitted() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header("nt-patch-mode", "append"))
            .and(|req: &Request| !req.headers.contains_key("if-match"))
            .and(|req: &Request| !req.headers.contains_key("content-range"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, append_args(None)).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ok_response_returns_etag() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header("nt-patch-mode", "append"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"next\"")
                    .insert_header("Location", "/v1/knowledgebases/notes/hello.md"),
            )
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, append_args(None)).await.unwrap();
        let rendered = format!("{result:?}");

        assert!(rendered.contains("next"), "etag missing: {rendered}");
    }

    #[tokio::test]
    async fn precondition_failed_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header_exists("nt-patch-mode"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error": "precondition_failed",
                "message": "etag mismatch"
            })))
            .mount(&server)
            .await;

        let c = client(&server.uri());

        assert!(run(&c, append_args(Some("\"old\"".into()))).await.is_err());
    }

    #[tokio::test]
    async fn not_found_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .and(header_exists("nt-patch-mode"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "object not found"
            })))
            .mount(&server)
            .await;

        let c = client(&server.uri());

        assert!(run(&c, append_args(None)).await.is_err());
    }

    #[tokio::test]
    async fn nested_path_is_encoded_once() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"))
            .and(header("nt-patch-mode", "append"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"abc123\"")
                    .insert_header("Location", "/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let args = AppendArgs {
            kb: "notes".into(),
            path: "docs/rfc/7231.md".into(),
            content: "\n# RFC 7231".into(),
            if_match: None,
        };
        let result = run(&c, args).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }
}
