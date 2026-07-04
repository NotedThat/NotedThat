use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use rmcp::{ErrorData as McpError, model::*};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListKbsArgs {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct KbEntry {
    pub kb_slug: String,
}

#[derive(Debug, Deserialize)]
struct ListKbsResponse {
    knowledgebases: Vec<String>,
}

pub(super) async fn run(client: &NotedThatClient) -> Result<CallToolResult, McpError> {
    let url = client.v1_url(&["knowledgebases"]);
    let resp = client
        .authorized(client.http.get(url))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let body: ListKbsResponse = resp
        .json()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;
    let entries: Vec<KbEntry> = body
        .knowledgebases
        .into_iter()
        .map(|kb_slug| KbEntry { kb_slug })
        .collect();
    Ok(CallToolResult::success(vec![ContentBlock::json(entries)?]))
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

    #[tokio::test]
    async fn happy_returns_entries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"knowledgebases": ["notes", "scratch"]})),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let result = run(&c).await.unwrap();
        assert!(!result.content.is_empty());
        let json_str = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let entries: Vec<KbEntry> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kb_slug, "notes");
        assert_eq!(entries[1].kb_slug, "scratch");
    }

    #[tokio::test]
    async fn schema_has_only_kb_slug() {
        let schema = schemars::schema_for!(KbEntry);
        let schema_json = serde_json::to_value(&schema).unwrap();
        let props = schema_json
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .unwrap();
        assert!(props.contains_key("kb_slug"));
        assert!(!props.contains_key("display_name"));
        assert!(!props.contains_key("description"));
        assert!(!props.contains_key("perms"));
        assert_eq!(props.len(), 1);
    }

    #[tokio::test]
    async fn unauthorized_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error":"unauthorized","message":"bad token","request_id":"r1"
            })))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let result = run(&c).await;
        assert!(result.is_err());
    }
}
