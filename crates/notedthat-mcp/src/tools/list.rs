use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{ErrorData as McpError, model::{CallToolResult, ContentBlock}};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    pub kb: String,
    pub prefix: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: ListArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.v1_url(&["knowledgebases", &kb_enc]);
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(prefix) = args.prefix {
        query.push(("prefix", prefix));
    }
    if let Some(limit) = args.limit {
        query.push(("limit", limit.to_string()));
    }
    if let Some(cursor) = args.cursor {
        query.push(("cursor", cursor));
    }

    let resp = client
        .authorized(client.http.get(url).query(&query))
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
        matchers::{method, path, query_param},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    #[tokio::test]
    async fn prefix_passed_as_query_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes"))
            .and(query_param("prefix", "docs/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"objects":[],"truncated":false})),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ListArgs {
            kb: "notes".into(),
            prefix: Some("docs/".into()),
            limit: None,
            cursor: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn cursor_passed_through() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes"))
            .and(query_param("cursor", "opaque-token-xyz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"objects":[],"truncated":false})),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ListArgs {
            kb: "notes".into(),
            prefix: None,
            limit: None,
            cursor: Some("opaque-token-xyz".into()),
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn missing_kb_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/nonexistent"))
            .respond_with(ResponseTemplate::new(404).set_body_json(
                serde_json::json!({"error":"not_found","message":"KB not declared"}),
            ))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ListArgs {
            kb: "nonexistent".into(),
            prefix: None,
            limit: None,
            cursor: None,
        };
        assert!(run(&c, args).await.is_err());
    }
}
