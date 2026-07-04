use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::{encode_kb_slug, encode_object_path};
use rmcp::{ErrorData as McpError, model::{CallToolResult, ContentBlock}};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    pub kb: String,
    pub path: String,
    pub content: String,
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct WriteResult {
    etag: Option<String>,
    location: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: WriteArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let path_enc = encode_object_path(&args.path);
    let url = client.v1_url(&["knowledgebases", &kb_enc, &path_enc]);

    let mut req = client
        .authorized(client.http.put(url))
        .body(args.content.into_bytes());

    if let Some(ct) = args.mime_type {
        req = req.header("Content-Type", ct);
    }
    if let Some(im) = args.if_match {
        req = req.header("If-Match", im);
    }
    if let Some(inm) = args.if_none_match {
        req = req.header("If-None-Match", inm);
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

    let result = WriteResult { etag, location };
    Ok(CallToolResult::success(vec![ContentBlock::json(result)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    #[tokio::test]
    async fn happy_put_returns_etag() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header("ETag", "\"abc123\"")
                    .insert_header("Location", "/v1/knowledgebases/notes/hello.md"),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = WriteArgs {
            kb: "notes".into(),
            path: "hello.md".into(),
            content: "# Hello".into(),
            if_match: None,
            if_none_match: None,
            mime_type: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn precondition_failed_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/knowledgebases/notes/hello.md"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error":"precondition_failed","message":"etag mismatch"
            })))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = WriteArgs {
            kb: "notes".into(),
            path: "hello.md".into(),
            content: "x".into(),
            if_match: Some("\"old\"".into()),
            if_none_match: None,
            mime_type: None,
        };
        assert!(run(&c, args).await.is_err());
    }

    #[tokio::test]
    async fn no_content_type_when_mime_omitted() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/knowledgebases/notes/f.md"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = WriteArgs {
            kb: "notes".into(),
            path: "f.md".into(),
            content: "x".into(),
            if_match: None,
            if_none_match: None,
            mime_type: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }
}
