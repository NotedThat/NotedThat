use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use reqwest::header::HeaderMap;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// Knowledge base slug.
    pub kb: String,
    /// Object path within the knowledge base, e.g. `notes/hello.md`.
    pub path: String,
    /// Optional first byte to read (0-based inclusive). Use with `byte_end` for a range.
    pub byte_start: Option<u64>,
    /// Optional end of the byte range, exclusive. Requires `byte_start`.
    pub byte_end: Option<u64>,
    /// Optional first line number to read (1-based inclusive). Use with `line_end` for a range.
    pub line_start: Option<u64>,
    /// Optional last line number to read (1-based inclusive). Requires `line_start`. Set to `line_start - 1` to signal an insert point.
    pub line_end: Option<u64>,
}

/// What the read returned and which version it belongs to, from the same HTTP
/// response as the text — never from a separate `list` or `HEAD`, whose answer
/// may describe a newer version than the text in hand.
///
/// Every field but `bytes_returned` is `null` when the API did not say: nothing
/// here is invented. `byte_end` is exclusive, like the argument of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadMeta {
    /// The `ETag` of the version this text was read from, verbatim (quoted) — pass
    /// it as `if_match` to `edit` or `replace` so a concurrent change is refused
    /// rather than overwritten.
    pub etag: Option<String>,
    /// The object's content type.
    pub content_type: Option<String>,
    /// Bytes in `content` — the slice returned, not the object.
    pub bytes_returned: u64,
    /// The whole object's size in bytes.
    pub total_bytes: Option<u64>,
    /// First byte of the returned slice (0-based inclusive).
    pub byte_start: Option<u64>,
    /// End of the returned slice, exclusive.
    pub byte_end: Option<u64>,
    /// The whole object's line count; line reads only.
    pub total_lines: Option<u64>,
    /// First line of the returned slice (1-based inclusive); line reads only.
    pub line_start: Option<u64>,
    /// Last line of the returned slice (1-based inclusive); line reads only. One less
    /// than `line_start` for an insert point.
    pub line_end: Option<u64>,
}

impl ReadMeta {
    /// Read the metadata off a successful response's headers.
    ///
    /// Which header says what (see `docs/API.md` § Range reads):
    /// - `200`: `Content-Length` is the object's size; the slice is the object.
    /// - byte `206`: `Content-Range: bytes a-b/N`, `b` inclusive.
    /// - line `206`: `Content-Range: lines a-b/N` plus `X-Content-Range-Bytes: s-e/N`,
    ///   `e` inclusive — `e = s - 1` for an insert point, an empty slice.
    ///
    /// A header that is missing or unparseable leaves its fields `null`; it is never
    /// a failure, since the text itself arrived.
    fn from_response(
        status: reqwest::StatusCode,
        headers: &HeaderMap,
        bytes_returned: u64,
    ) -> Self {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let mut meta = Self {
            etag: header("etag").map(str::to_owned),
            content_type: header("content-type").map(str::to_owned),
            bytes_returned,
            total_bytes: None,
            byte_start: None,
            byte_end: None,
            total_lines: None,
            line_start: None,
            line_end: None,
        };

        if status != reqwest::StatusCode::PARTIAL_CONTENT {
            if let Some(total) = header("content-length").and_then(|v| v.parse::<u64>().ok()) {
                meta.total_bytes = Some(total);
                meta.byte_start = Some(0);
                meta.byte_end = Some(total);
            }
            return meta;
        }

        match header("content-range").and_then(|v| v.split_once(' ')) {
            Some(("bytes", spec)) => {
                if let Some((start, end_inclusive, total)) = parse_range_spec(spec) {
                    meta.byte_start = Some(start);
                    meta.byte_end = Some(exclusive_end(start, end_inclusive));
                    meta.total_bytes = Some(total);
                }
            }
            Some(("lines", spec)) => {
                if let Some((start, end, total)) = parse_range_spec(spec) {
                    meta.line_start = Some(start);
                    meta.line_end = Some(end);
                    meta.total_lines = Some(total);
                }
                if let Some((start, end_inclusive, total)) =
                    header("x-content-range-bytes").and_then(parse_range_spec)
                {
                    meta.byte_start = Some(start);
                    meta.byte_end = Some(exclusive_end(start, end_inclusive));
                    meta.total_bytes = Some(total);
                }
            }
            _ => {}
        }
        meta
    }
}

/// `a-b/N` → `(a, b, N)`. Tolerates nothing: any other shape is `None`.
fn parse_range_spec(spec: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = spec.trim().split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

/// An inclusive header end as the exclusive end the tool speaks in. An insert point
/// arrives as `end = start - 1`, which is the empty slice `[start, start)`.
fn exclusive_end(start: u64, end_inclusive: u64) -> u64 {
    if end_inclusive < start {
        start
    } else {
        end_inclusive + 1
    }
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
    let url = client.api_v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let mut req = client.authorized(client.http.get(url));
    if let Some(range) = range_header {
        req = req.header("Range", range);
    }

    let resp = req.send().await.map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = client
        .read_body_bounded(resp)
        .await
        .map_err(McpError::from)?;

    let meta = ReadMeta::from_response(status, &headers, bytes.len() as u64);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // Text and metadata side by side, on purpose: `CallToolResult::structured`
    // would copy the JSON into `content` as well, and a client would then have
    // to tell the object's text from its own description.
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(serde_json::to_value(meta).map_err(McpToolError::from)?);
    Ok(result)
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

    fn text_of(result: &CallToolResult) -> &str {
        assert_eq!(result.content.len(), 1, "one text block, nothing else");
        &result.content[0].as_text().expect("text block").text
    }

    fn meta_of(result: &CallToolResult) -> ReadMeta {
        let value = result
            .structured_content
            .clone()
            .expect("structuredContent");
        // Round-trip through the serialized shape a client sees.
        serde_json::from_value(value).expect("structuredContent shape")
    }

    fn no_range() -> ReadMeta {
        ReadMeta {
            etag: None,
            content_type: None,
            bytes_returned: 0,
            total_bytes: None,
            byte_start: None,
            byte_end: None,
            total_lines: None,
            line_start: None,
            line_end: None,
        }
    }

    #[tokio::test]
    async fn a_full_read_carries_its_own_etag_and_the_object_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("# Hello\n", "text/markdown")
                    .insert_header("ETag", "\"abc123\""),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = run(&client(&server.uri()), file_args()).await.unwrap();
        assert_eq!(text_of(&result), "# Hello\n");
        assert_eq!(
            meta_of(&result),
            ReadMeta {
                etag: Some("\"abc123\"".into()),
                content_type: Some("text/markdown".into()),
                bytes_returned: 8,
                total_bytes: Some(8),
                byte_start: Some(0),
                byte_end: Some(8),
                ..no_range()
            }
        );
        assert_eq!(result.is_error, Some(false));
        server.verify().await;
    }

    #[tokio::test]
    async fn a_line_read_carries_line_and_byte_totals() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=1-5"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("ETag", "\"v1\"")
                    .insert_header("Content-Range", "lines 1-5/20")
                    .insert_header("X-Content-Range-Bytes", "0-149/400")
                    .set_body_string("one\ntwo\nthree\nfour\nfive\n"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = ReadArgs {
            line_start: Some(1),
            line_end: Some(5),
            ..file_args()
        };
        let result = run(&client(&server.uri()), args).await.unwrap();
        assert_eq!(
            meta_of(&result),
            ReadMeta {
                etag: Some("\"v1\"".into()),
                content_type: Some("text/plain".into()),
                bytes_returned: 24,
                total_bytes: Some(400),
                byte_start: Some(0),
                byte_end: Some(150),
                total_lines: Some(20),
                line_start: Some(1),
                line_end: Some(5),
            }
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn an_insert_point_read_is_an_empty_slice_at_its_offset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=5-4"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "lines 5-4/20")
                    .insert_header("X-Content-Range-Bytes", "150-149/400")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = ReadArgs {
            line_start: Some(5),
            line_end: Some(4),
            ..file_args()
        };
        let result = run(&client(&server.uri()), args).await.unwrap();
        assert_eq!(text_of(&result), "");
        assert_eq!(
            meta_of(&result),
            ReadMeta {
                content_type: Some("text/plain".into()),
                bytes_returned: 0,
                total_bytes: Some(400),
                byte_start: Some(150),
                byte_end: Some(150),
                total_lines: Some(20),
                line_start: Some(5),
                line_end: Some(4),
                ..no_range()
            }
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn nothing_is_invented_when_the_api_says_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .and(header("range", "bytes=0-9"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes zero-nine/lots")
                    .set_body_string("0123456789"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = ReadArgs {
            byte_start: Some(0),
            byte_end: Some(10),
            ..file_args()
        };
        let result = run(&client(&server.uri()), args).await.unwrap();
        assert_eq!(text_of(&result), "0123456789");
        assert_eq!(
            meta_of(&result),
            ReadMeta {
                content_type: Some("text/plain".into()),
                bytes_returned: 10,
                ..no_range()
            }
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn a_declared_oversized_body_is_refused_before_it_is_read() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 2048]))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri()).with_max_read_bytes(1024);
        let error = run(&c, file_args()).await.unwrap_err();
        assert!(
            error
                .message
                .starts_with("response_too_large: the object is 2048 bytes"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("byte_start/byte_end"),
            "{}",
            error.message
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_budget_is_read() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1024]))
            .mount(&server)
            .await;
        let c = client(&server.uri()).with_max_read_bytes(1024);
        let result = run(&c, file_args()).await.unwrap();
        assert_eq!(meta_of(&result).bytes_returned, 1024);
    }

    /// A body with no `Content-Length` — chunked, or a proxy that dropped it — is
    /// refused the moment it crosses the budget. `wiremock` always declares a
    /// length, so this serves a stream that never ends: the only way the call
    /// returns `response_too_large` (rather than the client's timeout) is by the
    /// reader stopping at the budget.
    #[tokio::test]
    async fn an_undeclared_oversized_body_is_refused_while_streaming() {
        use axum::{Router, body::Body, routing::get};

        let app = Router::new().route(
            "/api/v1/knowledgebases/kb/file.md",
            get(|| async {
                Body::from_stream(futures::stream::repeat_with(|| {
                    Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; 256]))
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let c = client(&format!("http://{addr}")).with_max_read_bytes(1024);
        let error = run(&c, file_args()).await.unwrap_err();
        assert!(
            error
                .message
                .starts_with("response_too_large: the object is larger than"),
            "no size invented for an undeclared length: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn exclusive_to_inclusive_conversion() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
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
        assert_eq!(text_of(&result), "0123456789");
        assert_eq!(
            meta_of(&result),
            ReadMeta {
                bytes_returned: 10,
                total_bytes: Some(100),
                byte_start: Some(0),
                byte_end: Some(10),
                ..no_range()
            }
        );
    }

    #[tokio::test]
    async fn range_not_satisfiable_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
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
            .and(path("/api/v1/knowledgebases/kb/file.md"))
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
            .and(path("/api/v1/knowledgebases/kb/file.md"))
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
            .and(path("/api/v1/knowledgebases/kb/file.md"))
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
        // `api_v1_url` encodes the `%` again via PATH_SEGMENT push, resulting in
        // `docs%252Frfc%252F7231.md` on the wire. This mock matches the CORRECT
        // single-encoded path `/api/v1/knowledgebases/notes/docs%2Frfc%2F7231.md`.
        // Before the fix: server.verify() FAILS because the mock is never called.
        // After the fix (Task 2): this test PASSES.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"))
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
            .and(path("/api/v1/knowledgebases/notes/docs%2Frfc%2F7231.md"))
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
