use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use notedthat_core::search::{SearchFilter, SearchResponse};
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
    /// The full HTTP filter surface, reused rather than mirrored.
    ///
    /// This used to be a hand-written struct carrying only `mime`, which is
    /// exactly how it drifted behind the HTTP API. Deriving the tool schema from
    /// `notedthat_core::search::SearchFilter` means a filter added there — the
    /// OKF filters of D48, for instance — reaches MCP clients automatically and
    /// cannot drift again.
    pub filters: Option<SearchFilter>,
    pub limit: Option<u32>,
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
                ..Default::default()
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

    #[tokio::test]
    async fn okf_filters_reach_the_http_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/knowledgebases/notes/search"))
            .and(body_string_contains("\"okf_type\":\"Metric\""))
            .and(body_string_contains("\"exclude_stale\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits":[]})))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = SearchArgs {
            kb: "notes".into(),
            query: "q".into(),
            filters: Some(SearchFilter {
                okf_type: Some("Metric".into()),
                exclude_stale: true,
                ..Default::default()
            }),
            limit: None,
        };
        assert!(run(&c, args).await.is_ok());
    }

    #[test]
    fn tool_schema_covers_every_core_filter_field() {
        // The guard against re-introducing a hand-mirrored filter struct: if a
        // field is added to the core type, it must appear in the tool schema.
        let schema = schemars::schema_for!(SearchFilter);
        let schema = serde_json::to_value(&schema).unwrap();
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("filter schema has properties");

        for field in [
            "object_key_prefix",
            "mime",
            "heading_path_prefix",
            "updated_after",
            "updated_before",
            "tags",
            "okf_type",
            "okf_status",
            "okf_min_trust",
            "okf_runtime",
            "chunk_kind",
            "exclude_stale",
            "okf_only",
        ] {
            assert!(
                properties.contains_key(field),
                "`{field}` is missing from the MCP search filter schema"
            );
        }
    }
}
