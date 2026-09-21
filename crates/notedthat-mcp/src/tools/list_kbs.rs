use crate::client::NotedThatClient;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListKbsArgs {}

/// One listed knowledge base: the slug the other tools take, the manifest's
/// display name and, when the manifest says, what the base is for — so an
/// agent can choose where to search before it searches (#98).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct KbEntry {
    pub kb_slug: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub(super) async fn run(client: &NotedThatClient) -> Result<CallToolResult, McpError> {
    let entries: Vec<KbEntry> = client
        .list_kbs()
        .await?
        .into_iter()
        .map(|kb| KbEntry {
            display_name: kb.display_name.unwrap_or_else(|| kb.kb_slug.clone()),
            kb_slug: kb.kb_slug,
            description: kb.description,
        })
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
            .and(path("/api/v1/knowledgebases"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "knowledgebases": [
                    {
                        "kb_slug": "notes",
                        "display_name": "Notes",
                        "description": "Engineering notes and ADRs."
                    },
                    { "kb_slug": "scratch", "display_name": "scratch" }
                ]
            })))
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
        assert_eq!(entries[0].display_name, "Notes");
        assert_eq!(
            entries[0].description.as_deref(),
            Some("Engineering notes and ADRs.")
        );
        assert_eq!(entries[1].kb_slug, "scratch");
        assert_eq!(entries[1].display_name, "scratch");
        assert_eq!(entries[1].description, None);
        // An absent description is absent, not `null`: the entry an agent
        // reads is exactly what the manifest says.
        let raw: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(raw[1].get("description").is_none());
    }

    /// A server from before #98 lists bare slugs. The adapter still works
    /// against it — the slug doubles as the display name, as the server does
    /// for a manifest without one — instead of failing every listing with an
    /// opaque deserialization error.
    #[tokio::test]
    async fn a_pre_98_server_listing_bare_slugs_is_still_listed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"knowledgebases": ["notes", "scratch"]})),
            )
            .mount(&server)
            .await;
        let result = run(&client(&server.uri())).await.unwrap();
        let json_str = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let entries: Vec<KbEntry> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kb_slug, "notes");
        assert_eq!(entries[0].display_name, "notes");
        assert_eq!(entries[0].description, None);
    }

    #[tokio::test]
    async fn schema_names_slug_display_name_and_description() {
        let schema = schemars::schema_for!(KbEntry);
        let schema_json = serde_json::to_value(&schema).unwrap();
        let props = schema_json
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .unwrap();
        assert!(props.contains_key("kb_slug"));
        assert!(props.contains_key("display_name"));
        assert!(props.contains_key("description"));
        // `perms` is what §6.10 promises and v1 does not return.
        assert!(!props.contains_key("perms"));
        assert_eq!(props.len(), 3);
    }

    #[tokio::test]
    async fn unauthorized_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases"))
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
