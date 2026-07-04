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
pub struct ReadArgs {
    pub kb: String,
    pub path: String,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: ReadArgs,
) -> Result<CallToolResult, McpError> {
    let range_header: Option<String> = match (args.byte_start, args.byte_end) {
        (None, None) => None,
        (Some(start), None) => Some(format!("bytes={start}-")),
        (Some(start), Some(end)) => {
            if start >= end {
                return Err(McpToolError::InvalidRequest(format!(
                    "byte_start ({start}) must be less than byte_end ({end})"
                ))
                .into());
            }
            Some(format!("bytes={start}-{}", end - 1))
        }
        (None, Some(_)) => {
            return Err(McpToolError::InvalidRequest(
                "byte_end requires byte_start; provide both or omit both".into(),
            )
            .into());
        }
    };

    let kb_enc = encode_kb_slug(&args.kb);
    // NOTE: url::push() uses PATH_SEGMENT encoding and leaves : @ [ ] ^ | ! $ & ' ( ) * + , ; = and sub-delims unencoded; ObjectPath accepts these.
    let url = client.v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let mut req = client.authorized(client.http.get(url));
    if let Some(range) = range_header {
        req = req.header("Range", range);
    }

    let resp = req.send().await.map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let bytes = resp
        .bytes()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;

    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
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
        NotedThatClient::new(url, "tok").unwrap()
    }

    #[tokio::test]
    async fn exclusive_to_inclusive_conversion() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/file.md"))
            .and(header("range", "bytes=0-9"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-9/100")
                    .set_body_bytes(b"0123456789".to_vec()),
            )
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            kb: "kb".into(),
            path: "file.md".into(),
            byte_start: Some(0),
            byte_end: Some(10),
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn range_not_satisfiable_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/file.md"))
            .respond_with(ResponseTemplate::new(416).insert_header("Content-Range", "bytes */50"))
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            kb: "kb".into(),
            path: "file.md".into(),
            byte_start: Some(1000),
            byte_end: Some(2000),
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn byte_end_alone_rejected_no_http_call() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            kb: "kb".into(),
            path: "file.md".into(),
            byte_start: None,
            byte_end: Some(100),
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn equal_start_end_rejected() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            kb: "kb".into(),
            path: "file.md".into(),
            byte_start: Some(10),
            byte_end: Some(10),
        };
        assert!(run(&c, args).await.is_err());
    }

    #[tokio::test]
    async fn read_nested_path_wiremock_red_gate() {
        // RED GATE: Before the fix, the read tool pre-encodes `docs/rfc/7231.md`
        // via `encode_object_path` producing `docs%2Frfc%2F7231.md`, then
        // `v1_url` encodes the `%` again via PATH_SEGMENT push, resulting in
        // `docs%252Frfc%252F7231.md` on the wire. This mock matches the CORRECT
        // single-encoded path `/v1/knowledgebases/notes/docs%2Frfc%2F7231.md`.
        // Before the fix: server.verify() FAILS because the mock is never called.
        // After the fix (Task 2): this test PASSES.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# RFC 7231"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            kb: "notes".into(),
            path: "docs/rfc/7231.md".into(),
            byte_start: None,
            byte_end: None,
        };
        // Call the tool — before fix this sends the wrong (double-encoded) URL.
        // We ignore the result; what matters is whether the mock was called.
        let _ = run(&c, args).await;
        server.verify().await; // FAILS before fix (mock not matched), PASSES after fix
    }

    #[tokio::test]
    async fn nested_path_is_encoded_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# RFC 7231"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            kb: "notes".into(),
            path: "docs/rfc/7231.md".into(),
            byte_start: None,
            byte_end: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }
}
