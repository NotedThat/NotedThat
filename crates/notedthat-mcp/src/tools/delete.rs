use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{ErrorData as McpError, model::{CallToolResult, ContentBlock}};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteArgs {
    pub kb: String,
    pub path: String,
    pub if_match: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: DeleteArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    // NOTE: url::push() uses PATH_SEGMENT encoding and leaves : @ [ ] ^ | ! $ & ' ( ) * + , ; = and sub-delims unencoded; ObjectPath accepts these.
    let url = client.v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let mut req = client.authorized(client.http.delete(url));
    if let Some(if_match) = args.if_match {
        req = req.header("If-Match", if_match);
    }

    let resp = req.send().await.map_err(McpToolError::Transport)?;
    map_response(resp).await.map_err(McpError::from)?;
    Ok(CallToolResult::success(vec![ContentBlock::text("deleted")]))
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
    async fn happy_delete_204() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/knowledgebases/notes/a.md"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = DeleteArgs {
            kb: "notes".into(),
            path: "a.md".into(),
            if_match: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn precondition_failed() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/knowledgebases/notes/a.md"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error":"precondition_failed","message":"etag mismatch"
            })))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = DeleteArgs {
            kb: "notes".into(),
            path: "a.md".into(),
            if_match: Some("\"stale\"".into()),
        };
        assert!(run(&c, args).await.is_err());
    }

    #[tokio::test]
    async fn nested_path_is_encoded_once() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = DeleteArgs {
            kb: "notes".into(),
            path: "docs/rfc/7231.md".into(),
            if_match: Some("\"etag1\"".into()),
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }
}
