//! `index_status(kb)`: one knowledge base's search-index health, as
//! `GET /api/v1/knowledgebases/{kb}/index` reports it (#97).

use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexStatusArgs {
    /// The knowledge base slug.
    pub kb: String,
}

/// The API's answer is passed through unchanged: the state model is the
/// API's, documented once, and an agent reading either surface sees one shape.
pub(super) async fn run(
    client: &NotedThatClient,
    args: IndexStatusArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.api_v1_url(&["knowledgebases", &kb_enc, "index"]);
    let resp = client
        .authorized(client.http.get(url))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;
    Ok(CallToolResult::success(vec![ContentBlock::json(body)?]))
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
        NotedThatClient::new(url, "test-token").unwrap()
    }

    fn output(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            ContentBlock::Text(t) => serde_json::from_str(&t.text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn the_apis_view_is_passed_through_unchanged() {
        let server = MockServer::start().await;
        let view = serde_json::json!({
            "kb_slug": "notes",
            "state": "failed",
            "pending": 0,
            "queue": { "depth": 0, "capacity": 1024 },
            "worker": "running",
            "last_indexed_at": "2026-09-21T01:02:03Z",
            "last_failure": {
                "at": "2026-09-21T01:05:00Z",
                "object_key": "a.md",
                "summary": "embedder.embed failed: connection refused"
            },
            "last_reconcile": null
        });
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/notes/index"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(view.clone()))
            .mount(&server)
            .await;

        let result = run(
            &client(&server.uri()),
            IndexStatusArgs { kb: "notes".into() },
        )
        .await
        .unwrap();

        assert_eq!(output(&result), view);
    }

    #[tokio::test]
    async fn a_refusal_is_the_apis_refusal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/private/index"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": "forbidden", "message": "forbidden", "request_id": "r1"
            })))
            .mount(&server)
            .await;

        let err = run(
            &client(&server.uri()),
            IndexStatusArgs {
                kb: "private".into(),
            },
        )
        .await
        .unwrap_err();

        assert_eq!(err.message, "forbidden");
    }

    #[tokio::test]
    async fn an_undeclared_knowledge_base_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/nope/index"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found", "message": "knowledge base not found: nope", "request_id": "r1"
            })))
            .mount(&server)
            .await;

        let err = run(
            &client(&server.uri()),
            IndexStatusArgs { kb: "nope".into() },
        )
        .await
        .unwrap_err();

        assert!(err.message.contains("not found"), "{}", err.message);
    }
}
