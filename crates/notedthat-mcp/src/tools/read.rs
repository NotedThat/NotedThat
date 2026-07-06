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
    /// Optional first line number to read (1-based inclusive). Use with `line_end` for a range.
    pub line_start: Option<u64>,
    /// Optional last line number to read (1-based inclusive). Requires `line_start`. Set to `line_start - 1` to signal an insert point.
    pub line_end: Option<u64>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: ReadArgs,
) -> Result<CallToolResult, McpError> {
    let byte_range_requested = args.byte_start.is_some() || args.byte_end.is_some();
    let line_range_requested = args.line_start.is_some() || args.line_end.is_some();
    if byte_range_requested && line_range_requested {
        return Err(McpToolError::InvalidRequest(
            "byte_* and line_* arguments are mutually exclusive; provide one pair or the other"
                .into(),
        )
        .into());
    }
    if args.line_end.is_some() && args.line_start.is_none() {
        return Err(McpToolError::InvalidRequest(
            "line_end requires line_start; provide both or omit both".into(),
        )
        .into());
    }
    if args.line_start == Some(0) {
        return Err(McpToolError::InvalidRequest(
            "line numbers are 1-based; line_start must be >= 1".into(),
        )
        .into());
    }
    if let (Some(start), Some(end)) = (args.line_start, args.line_end)
        && start > end
        && start != end + 1
    {
        return Err(McpToolError::InvalidRequest(
            "line_start must be <= line_end + 1; see docs for insert-point encoding".into(),
        )
        .into());
    }

    let range_header: Option<String> = match (
        args.byte_start,
        args.byte_end,
        args.line_start,
        args.line_end,
    ) {
        (None, None, None, None) => None,
        (Some(start), None, None, None) => Some(format!("bytes={start}-")),
        (Some(start), Some(end), None, None) => {
            if start >= end {
                return Err(McpToolError::InvalidRequest(format!(
                    "byte_start ({start}) must be less than byte_end ({end})"
                ))
                .into());
            }
            Some(format!("bytes={start}-{}", end - 1))
        }
        (None, Some(_), None, None) => {
            return Err(McpToolError::InvalidRequest(
                "byte_end requires byte_start; provide both or omit both".into(),
            )
            .into());
        }
        (None, None, Some(start), None) => Some(format!("lines={start}-")),
        (None, None, Some(start), Some(end)) => Some(format!("lines={start}-{end}")),
        _ => unreachable!("range validation rejected mixed or incomplete line ranges"),
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

    fn file_args() -> ReadArgs {
        ReadArgs {
            kb: "kb".into(),
            path: "file.md".into(),
            byte_start: None,
            byte_end: None,
            line_start: None,
            line_end: None,
        }
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
            byte_start: Some(0),
            byte_end: Some(10),
            ..file_args()
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
            byte_start: Some(1000),
            byte_end: Some(2000),
            ..file_args()
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn byte_end_alone_rejected_no_http_call() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            byte_end: Some(100),
            ..file_args()
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
            byte_start: Some(10),
            byte_end: Some(10),
            ..file_args()
        };
        assert!(run(&c, args).await.is_err());
    }

    #[tokio::test]
    async fn line_range_sends_range_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=1-5"))
            .respond_with(ResponseTemplate::new(206).set_body_string("one\ntwo\nthree\nfour\nfive"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            line_start: Some(1),
            line_end: Some(5),
            ..file_args()
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn line_end_alone_rejected_no_http_call() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            line_end: Some(10),
            ..file_args()
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn byte_and_line_ranges_rejected_no_http_call() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            byte_start: Some(0),
            byte_end: Some(10),
            line_start: Some(1),
            line_end: Some(5),
            ..file_args()
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn zero_line_start_rejected_no_http_call() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            line_start: Some(0),
            ..file_args()
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn non_insert_descending_line_range_rejected_no_http_call() {
        let server = MockServer::start().await;
        let c = client(&server.uri());
        let args = ReadArgs {
            line_start: Some(5),
            line_end: Some(3),
            ..file_args()
        };
        let result = run(&c, args).await;
        assert!(result.is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn line_start_without_line_end_sends_open_range_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=3-"))
            .respond_with(ResponseTemplate::new(206).set_body_string("three\nfour"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            line_start: Some(3),
            ..file_args()
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn insert_point_line_range_is_accepted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=5-4"))
            .respond_with(ResponseTemplate::new(206).set_body_string(""))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri());
        let args = ReadArgs {
            line_start: Some(5),
            line_end: Some(4),
            ..file_args()
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
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
            line_start: None,
            line_end: None,
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
            line_start: None,
            line_end: None,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }
}
