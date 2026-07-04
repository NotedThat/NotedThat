use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use notedthat_core::search::SearchResponse;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub kb: String,
    pub query: String,
    pub filters: Option<SearchFilter>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchFilter {
    pub mime: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchBody {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<SearchFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: SearchArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.v1_url(&["knowledgebases", &kb_enc, "search"]);
    let body = SearchBody {
        query: args.query,
        filter: args.filters,
        limit: args.limit,
    };
    let resp = client
        .authorized(client.http.post(url).json(&body))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let search_resp: SearchResponse = resp
        .json()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;
    Ok(CallToolResult::success(vec![ContentBlock::json(
        search_resp,
    )?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    #[tokio::test]
    async fn happy_returns_hits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/search"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits":[{
                    "object_key":"a.md","byte_start":0,"byte_end":10,
                    "score":0.9,"preview":"hi"
                }]})),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = SearchArgs {
            kb: "notes".into(),
            query: "hello".into(),
            filters: None,
            limit: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn filter_field_renamed_to_singular() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/search"))
            .and(body_string_contains("\"filter\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits":[]})))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = SearchArgs {
            kb: "notes".into(),
            query: "q".into(),
            filters: Some(SearchFilter {
                mime: Some("text/markdown".into()),
            }),
            limit: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn kb_not_found_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/nonexistent/search"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error":"not_found","message":"KB missing"})),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = SearchArgs {
            kb: "nonexistent".into(),
            query: "q".into(),
            filters: None,
            limit: None,
        };
        assert!(run(&c, args).await.is_err());
    }
}
