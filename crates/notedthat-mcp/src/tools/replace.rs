use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Arguments for the replace tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceArgs {
    /// Knowledge base slug.
    pub kb: String,
    /// Object path within the knowledge base.
    pub path: String,
    /// Exact UTF-8 substring to find (verbatim bytes; must be non-empty).
    pub old_string: String,
    /// Replacement string (may be empty for delete-in-place).
    pub new_string: String,
    /// Required `ETag` from a previous GET or write (concurrency control).
    pub if_match: String,
    /// When true, replace every non-overlapping match. Default: false.
    #[serde(default)]
    pub replace_all: Option<bool>,
}

#[derive(Serialize)]
struct ReplaceResult {
    etag: Option<String>,
    match_count: u64,
    total_bytes: u64,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: ReplaceArgs,
) -> Result<CallToolResult, McpError> {
    if args.old_string.is_empty() {
        return Err(McpToolError::InvalidRequest(
            "old_string must be non-empty (would match every byte position)".into(),
        )
        .into());
    }
    if args.if_match.trim().is_empty() {
        return Err(McpToolError::InvalidRequest(
            "if_match is required and must be a strong ETag string".into(),
        )
        .into());
    }

    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.v1_url(&["knowledgebases", &kb_enc, "replace", &args.path]);

    let replace_all_flag = args.replace_all.unwrap_or(false);
    let body = serde_json::json!({
        "old_string": args.old_string,
        "new_string": args.new_string,
        "replace_all": replace_all_flag,
    });

    let req = client
        .authorized(client.http.post(url))
        .header("If-Match", &args.if_match)
        .header("Content-Type", "application/json")
        .body(body.to_string());

    let resp = req.send().await.map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;

    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let text = resp.text().await.map_err(McpToolError::Transport)?;
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        McpToolError::InternalError(format!("malformed replace response body: {e}"))
    })?;
    let match_count = parsed
        .get("match_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_bytes = parsed
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let result = ReplaceResult {
        etag,
        match_count,
        total_bytes,
    };
    Ok(CallToolResult::success(vec![ContentBlock::json(result)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    fn replace_args() -> ReplaceArgs {
        ReplaceArgs {
            kb: "notes".into(),
            path: "hello.md".into(),
            old_string: "world".into(),
            new_string: "planet".into(),
            if_match: "\"abc\"".into(),
            replace_all: None,
        }
    }

    #[tokio::test]
    async fn single_match_happy_sends_post_with_json_body_and_if_match() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/replace/hello.md"))
            .and(header("If-Match", "\"abc\""))
            .and(header("Content-Type", "application/json"))
            .and(body_string_contains("\"old_string\":\"world\""))
            .and(body_string_contains("\"new_string\":\"planet\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "etag": "\"new\"",
                "match_count": 1,
                "total_bytes": 12
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, replace_args()).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn replace_all_true_forwards_the_flag() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/replace/hello.md"))
            .and(body_string_contains("\"replace_all\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "etag": "\"new\"",
                "match_count": 2,
                "total_bytes": 12
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let args = ReplaceArgs {
            replace_all: Some(true),
            ..replace_args()
        };
        let result = run(&c, args).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn no_match_422_response_returns_no_match_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/replace/hello.md"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "no_match",
                "message": "world was not found"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let err = run(&c, replace_args()).await.unwrap_err();

        assert!(
            err.message.to_string().contains("no match found"),
            "expected NoMatch-shaped error, got: {err:?}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn ambiguous_match_422_response_returns_ambiguous_match_with_count() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/replace/hello.md"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "error": "ambiguous_match",
                "message": "world matched more than once",
                "match_count": 5
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let err = run(&c, replace_args()).await.unwrap_err();

        assert!(
            err.message.to_string().contains("multiple matches (5)"),
            "expected AmbiguousMatch-shaped error with count, got: {err:?}"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn missing_old_string_invalid_arg_returns_invalid_request_no_http_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let args = ReplaceArgs {
            old_string: String::new(),
            new_string: "x".into(),
            if_match: "\"e\"".into(),
            ..replace_args()
        };
        let err = run(&c, args).await.unwrap_err();

        assert_eq!(
            err.message.to_string(),
            "old_string must be non-empty (would match every byte position)"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn nested_path_is_encoded_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/v1/knowledgebases/notes/replace/docs%2Frfc%2F7231.md",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "etag": "\"new\"",
                "match_count": 1,
                "total_bytes": 12
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let args = ReplaceArgs {
            path: "docs/rfc/7231.md".into(),
            ..replace_args()
        };
        let result = run(&c, args).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn precondition_failed_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/replace/hello.md"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error": "precondition_failed",
                "message": "etag mismatch"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let err = run(&c, replace_args()).await.unwrap_err();

        assert_eq!(err.message.to_string(), "precondition_failed");
        server.verify().await;
    }
}
