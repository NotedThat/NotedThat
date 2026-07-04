use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::{encode_kb_slug, encode_object_path};
use rmcp::{ErrorData as McpError, model::{CallToolResult, ContentBlock}};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveArgs {
    pub kb: String,
    pub from: String,
    pub to: String,
    pub if_match: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: MoveArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let from_enc = encode_object_path(&args.from);
    let to_enc = encode_object_path(&args.to);

    let get_url = client.v1_url(&["knowledgebases", &kb_enc, &from_enc]);
    let mut get_req = client.authorized(client.http.get(get_url));
    if let Some(ref if_match) = args.if_match {
        get_req = get_req.header("If-Match", if_match.as_str());
    }
    let get_resp = get_req.send().await.map_err(McpToolError::Transport)?;
    let get_resp = map_response(get_resp).await.map_err(McpError::from)?;

    let content_type = get_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let source_etag = get_resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body_bytes = get_resp
        .bytes()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;

    let put_url = client.v1_url(&["knowledgebases", &kb_enc, &to_enc]);
    let mut put_req = client.authorized(client.http.put(put_url)).body(body_bytes);
    if let Some(ct) = content_type {
        put_req = put_req.header("Content-Type", ct);
    }
    let put_resp = put_req.send().await.map_err(McpToolError::Transport)?;
    map_response(put_resp).await.map_err(McpError::from)?;

    let del_url = client.v1_url(&["knowledgebases", &kb_enc, &from_enc]);
    let mut del_req = client.authorized(client.http.delete(del_url));
    if let Some(etag) = source_etag {
        del_req = del_req.header("If-Match", etag);
    }
    let del_resp = del_req.send().await.map_err(McpToolError::Transport)?;
    if let Err(error) = map_response(del_resp).await {
        let msg = format!(
            "MOVE partially completed: destination created at {} but source deletion failed: {}; manually remove source at {}",
            args.to, error, args.from
        );
        return Err(McpToolError::InternalError(msg).into());
    }

    Ok(CallToolResult::success(vec![ContentBlock::text(format!(
        "moved {} -> {}",
        args.from, args.to
    ))]))
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
    async fn happy_move_three_calls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes/a.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/markdown")
                    .insert_header("ETag", "\"etag1\"")
                    .set_body_bytes(b"# a".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/v1/knowledgebases/notes/b.md"))
            .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"etag2\""))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/knowledgebases/notes/a.md"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = MoveArgs {
            kb: "notes".into(),
            from: "a.md".into(),
            to: "b.md".into(),
            if_match: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn source_not_found_no_put_or_delete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes/missing.md"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error":"not_found","message":"missing"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = MoveArgs {
            kb: "notes".into(),
            from: "missing.md".into(),
            to: "b.md".into(),
            if_match: None,
        };
        assert!(run(&c, args).await.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn partial_failure_message_contains_paths_not_request_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes/a.md"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/v1/knowledgebases/notes/b.md"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1/knowledgebases/notes/a.md"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error":"internal_error","message":"disk error","request_id":"SECRET_REQ"
            })))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = MoveArgs {
            kb: "notes".into(),
            from: "a.md".into(),
            to: "b.md".into(),
            if_match: None,
        };
        let err = run(&c, args).await.unwrap_err();
        let msg = err.message.to_string();
        assert!(msg.contains("MOVE partially completed"), "message: {msg}");
        assert!(msg.contains("a.md"), "message: {msg}");
        assert!(msg.contains("b.md"), "message: {msg}");
        assert!(!msg.contains("SECRET_REQ"), "request_id leaked: {msg}");
    }
}
