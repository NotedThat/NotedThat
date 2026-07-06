use crate::client::NotedThatClient;
use crate::error::McpToolError;
use rmcp::{ErrorData as McpError, model::CallToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

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
    /// Required ETag from a previous GET or write (concurrency control).
    pub if_match: String,
    /// When true, replace every non-overlapping match. Default: false.
    #[serde(default)]
    pub replace_all: Option<bool>,
}

pub(super) async fn run(
    _client: &NotedThatClient,
    args: ReplaceArgs,
) -> Result<CallToolResult, McpError> {
    // Client-side validation (needed to make test (e) pass correctly)
    if args.old_string.is_empty() {
        return Err(McpToolError::InvalidRequest(
            "old_string must be non-empty (would match every byte position)".into(),
        )
        .into());
    }
    Err(McpToolError::InvalidRequest("replace tool not implemented yet".into()).into())
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
        let result = run(&c, replace_args()).await;

        assert!(result.is_err());
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
        let result = run(&c, args).await;

        assert!(result.is_err());
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
        let result = run(&c, args).await;

        assert!(result.is_err());
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
