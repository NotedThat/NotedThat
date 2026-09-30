use super::range::{RangeArgs, ReadSpan};
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

// The tool's arguments, parsed: the range is one `ReadSpan`, so a call naming
// an end without its start, or both pairs, is refused while its arguments are
// deserialized and never reaches `run`. The published schema is
// `RawReadArgs`'s four flat range fields. Plain comments, not rustdoc: a doc
// here would become the input schema's description.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(try_from = "RawReadArgs")]
#[schemars(with = "RawReadArgs")]
pub struct ReadArgs {
    pub kb: String,
    pub path: String,
    pub span: ReadSpan,
}

// The tool's arguments as a client sends them.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RawReadArgs {
    /// Knowledge base slug.
    pub kb: String,
    /// Object path within the knowledge base, e.g. `notes/hello.md`.
    pub path: String,
    /// Optional first byte to read (0-based inclusive). Use with `byte_end` for a range, alone to read to the end; not with `line_*`.
    pub byte_start: Option<u64>,
    /// Optional end of the byte range, exclusive. Requires `byte_start`, and must exceed it.
    pub byte_end: Option<u64>,
    /// Optional first line number to read (1-based inclusive). Use with `line_end` for a range, alone to read to the end; not with `byte_*`.
    pub line_start: Option<u64>,
    /// Optional last line number to read (1-based inclusive). Requires `line_start`. Set to `line_start - 1` to signal an insert point.
    pub line_end: Option<u64>,
}

impl TryFrom<RawReadArgs> for ReadArgs {
    type Error = String;

    fn try_from(raw: RawReadArgs) -> Result<Self, Self::Error> {
        let span = ReadSpan::parse(RangeArgs {
            line_start: raw.line_start,
            line_end: raw.line_end,
            byte_start: raw.byte_start,
            byte_end: raw.byte_end,
        })?;
        Ok(Self {
            kb: raw.kb,
            path: raw.path,
            span,
        })
    }
}

/// The tool's `structuredContent`: the text **and** what the read returned about
/// it, so a host that hands the model only the structured half (some do, once a
/// tool declares an output schema) still hands it the document. `content[0]` is
/// the same text as plain `TextContent`, for hosts that show only that.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadResult {
    /// The text read — identical to the tool's `content[0]`.
    pub text: String,
    /// Which version the text belongs to, and how much of the object it is.
    #[serde(flatten)]
    pub meta: ReadMeta,
}

/// What the read returned and which version it belongs to, from the same HTTP
/// response as the text — never from a separate `list` or `HEAD`, whose answer
/// may describe a newer version than the text in hand.
///
/// `byte_end` is exclusive, like the argument of the same name, and is derived
/// from the body (`byte_start + bytes_returned`) rather than read off the
/// header, whose inclusive end cannot spell the empty slice at offset 0. A
/// header that is missing or malformed leaves its fields `null`; nothing here
/// is invented — except that a `200` *is* the whole object, so its totals are
/// known without a header.
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
    ///   `e` inclusive — or, for an insert point (an empty slice), `*/N` plus
    ///   `X-Insert-Offset: s`.
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
            // A `200` is the whole object: the body in hand is the size, with
            // or without a `Content-Length` (a chunked body has none).
            meta.total_bytes = Some(bytes_returned);
            meta.byte_start = Some(0);
            meta.byte_end = Some(bytes_returned);
            return meta;
        }

        // For a `206` the header says where the slice starts and how big the
        // object is; where the slice *ends* is the body's to say: a backend
        // whose header disagreed with its body must not be believed over the
        // body.
        let slice = |start: u64, total: u64, meta: &mut Self| {
            meta.byte_start = Some(start);
            meta.byte_end = Some(start.saturating_add(bytes_returned));
            meta.total_bytes = Some(total);
        };
        match header("content-range").and_then(|v| v.split_once(' ')) {
            Some(("bytes", spec)) => {
                if let Some((start, _end_inclusive, total)) = parse_range_spec(spec) {
                    slice(start, total, &mut meta);
                }
            }
            Some(("lines", spec)) => {
                if let Some((start, end, total)) = parse_range_spec(spec) {
                    meta.line_start = Some(start);
                    meta.line_end = Some(end);
                    meta.total_lines = Some(total);
                }
                match header("x-content-range-bytes") {
                    Some(spec) if spec.trim().starts_with('*') => {
                        // An insert point: no bytes, so no inclusive range; the
                        // offset travels in its own header.
                        if let Some(total) = parse_unsatisfied_total(spec) {
                            meta.total_bytes = Some(total);
                            if let Some(start) =
                                header("x-insert-offset").and_then(|v| v.trim().parse().ok())
                            {
                                slice(start, total, &mut meta);
                            }
                        }
                    }
                    Some(spec) => {
                        if let Some((start, _end_inclusive, total)) = parse_range_spec(spec) {
                            slice(start, total, &mut meta);
                        }
                    }
                    None => {}
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

/// `*/N` → `N`. Tolerates nothing: any other shape is `None`.
fn parse_unsatisfied_total(spec: &str) -> Option<u64> {
    spec.trim().strip_prefix("*/")?.parse().ok()
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: ReadArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    // NOTE: url::push() uses PATH_SEGMENT encoding and leaves : @ [ ] ^ | ! $ & ' ( ) * + , ; = and sub-delims unencoded; ObjectPath accepts these.
    let url = client.api_v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let mut req = client.authorized(client.http.get(url));
    if let Some(range) = args.span.range_header() {
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
    // The text goes in both halves. MCP asks that `structuredContent` and
    // `content` be functionally equivalent: a host that prefers the structured
    // half once a tool declares an output schema would otherwise receive the
    // ETag and byte counts and never the document. `CallToolResult::structured`
    // is not used because it would put the *JSON* in `content`, and a host that
    // shows only `content` would then show the document wrapped in its own
    // description.
    let mut result = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
    result.structured_content =
        Some(serde_json::to_value(ReadResult { text, meta }).map_err(McpToolError::from)?);
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

    /// Arguments as a client would send them, through the same parse rmcp runs.
    fn parse(range: serde_json::Value) -> Result<ReadArgs, String> {
        let mut value = serde_json::json!({"kb": "kb", "path": "file.md"});
        let serde_json::Value::Object(range) = range else {
            panic!("a range is an object: {range}");
        };
        value.as_object_mut().unwrap().extend(range);
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn file_args() -> ReadArgs {
        parse(serde_json::json!({})).unwrap()
    }

    fn range_args(range: serde_json::Value) -> ReadArgs {
        parse(range).unwrap()
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
                    // The header agrees with the body: 24 bytes, offsets 0–23.
                    .insert_header("X-Content-Range-Bytes", "0-23/400")
                    .set_body_string("one\ntwo\nthree\nfour\nfive\n"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = range_args(serde_json::json!({"line_start": 1, "line_end": 5}));
        let result = run(&client(&server.uri()), args).await.unwrap();
        assert_eq!(
            meta_of(&result),
            ReadMeta {
                etag: Some("\"v1\"".into()),
                content_type: Some("text/plain".into()),
                bytes_returned: 24,
                total_bytes: Some(400),
                byte_start: Some(0),
                byte_end: Some(24),
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
                    .insert_header("X-Content-Range-Bytes", "*/400")
                    .insert_header("X-Insert-Offset", "150")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = range_args(serde_json::json!({"line_start": 5, "line_end": 4}));
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
        let args = range_args(serde_json::json!({"byte_start": 0, "byte_end": 10}));
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

    /// An insert point before line 1 arrives as `X-Content-Range-Bytes: */N`
    /// with `X-Insert-Offset: 0`: an empty slice at offset 0, not a one-byte one.
    #[tokio::test]
    async fn an_insert_point_before_line_one_is_the_empty_slice_at_zero() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=1-0"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "lines 1-0/20")
                    .insert_header("X-Content-Range-Bytes", "*/400")
                    .insert_header("X-Insert-Offset", "0")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = range_args(serde_json::json!({"line_start": 1, "line_end": 0}));
        let result = run(&client(&server.uri()), args).await.unwrap();
        let meta = meta_of(&result);
        assert_eq!(meta.bytes_returned, 0);
        assert_eq!((meta.byte_start, meta.byte_end), (Some(0), Some(0)));
        assert_eq!((meta.line_start, meta.line_end), (Some(1), Some(0)));
        assert_eq!(meta.total_bytes, Some(400));
        server.verify().await;
    }

    /// An insert point whose offset header is missing still knows the object's
    /// size; where the empty slice sits is not invented.
    #[tokio::test]
    async fn an_insert_point_without_an_offset_leaves_the_slice_null() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .and(header("range", "lines=5-4"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "lines 5-4/20")
                    .insert_header("X-Content-Range-Bytes", "*/400")
                    .set_body_string(""),
            )
            .expect(1)
            .mount(&server)
            .await;
        let args = range_args(serde_json::json!({"line_start": 5, "line_end": 4}));
        let result = run(&client(&server.uri()), args).await.unwrap();
        let meta = meta_of(&result);
        assert_eq!(meta.total_bytes, Some(400));
        assert_eq!((meta.byte_start, meta.byte_end), (None, None));
        assert_eq!((meta.line_start, meta.line_end), (Some(5), Some(4)));
        server.verify().await;
    }

    /// A `200` is the whole object whether or not the response says how long
    /// it is: a chunked body has no `Content-Length`, and the size is still
    /// known exactly — it was just read.
    #[tokio::test]
    async fn a_full_read_without_content_length_still_knows_the_object_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("twelve bytes"))
            .expect(1)
            .mount(&server)
            .await;
        // wiremock always declares a length; the assertion is on the derivation
        // being independent of it, which `from_response` makes by construction.
        let headers = HeaderMap::new();
        let meta = ReadMeta::from_response(reqwest::StatusCode::OK, &headers, 12);
        assert_eq!(meta.total_bytes, Some(12));
        assert_eq!((meta.byte_start, meta.byte_end), (Some(0), Some(12)));

        let result = run(&client(&server.uri()), file_args()).await.unwrap();
        assert_eq!(meta_of(&result).total_bytes, Some(12));
        server.verify().await;
    }

    /// A header end the body contradicts — or one no `u64` arithmetic could
    /// honour — never reaches the client: the end is `start + bytes_returned`.
    #[test]
    fn the_slice_end_comes_from_the_body_not_the_header() {
        let mut headers = HeaderMap::new();
        headers.insert("content-range", "bytes 10-99999/500".parse().unwrap());
        let meta = ReadMeta::from_response(reqwest::StatusCode::PARTIAL_CONTENT, &headers, 5);
        assert_eq!((meta.byte_start, meta.byte_end), (Some(10), Some(15)));

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-range",
            "bytes 0-18446744073709551615/18446744073709551615"
                .parse()
                .unwrap(),
        );
        let meta = ReadMeta::from_response(reqwest::StatusCode::PARTIAL_CONTENT, &headers, 3);
        assert_eq!((meta.byte_start, meta.byte_end), (Some(0), Some(3)));
    }

    /// `structuredContent` carries the text too, so a host that hands the model
    /// only the structured half still hands it the document.
    #[tokio::test]
    async fn structured_content_carries_the_text_as_well_as_the_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/file.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Hello\n"))
            .mount(&server)
            .await;
        let result = run(&client(&server.uri()), file_args()).await.unwrap();
        let structured = result
            .structured_content
            .clone()
            .expect("structuredContent");
        assert_eq!(structured["text"], "# Hello\n");
        assert_eq!(structured["text"], text_of(&result));
        assert_eq!(structured["bytes_returned"], 8);
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
        let args = range_args(serde_json::json!({"byte_start": 0, "byte_end": 10}));
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
        let args = range_args(serde_json::json!({"byte_start": 1000, "byte_end": 2000}));
        let result = run(&c, args).await;
        assert!(result.is_err());
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
        let args = range_args(serde_json::json!({"line_start": 1, "line_end": 5}));
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
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
        let args = range_args(serde_json::json!({"line_start": 3}));
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
        let args = range_args(serde_json::json!({"line_start": 5, "line_end": 4}));
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
            span: ReadSpan::Whole,
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
            span: ReadSpan::Whole,
        };
        let result = run(&c, args).await.unwrap();
        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[test]
    fn a_range_that_breaks_a_rule_is_refused_while_parsing_with_that_rule() {
        let cases = [
            (
                serde_json::json!({"byte_start": 0, "byte_end": 10, "line_start": 1, "line_end": 5}),
                "line_* and byte_* arguments are mutually exclusive; provide one pair or the other",
            ),
            (
                serde_json::json!({"byte_end": 100}),
                "byte_end requires byte_start; provide both or omit both",
            ),
            (
                serde_json::json!({"line_end": 10}),
                "line_end requires line_start; provide both or omit both",
            ),
            (
                serde_json::json!({"byte_start": 10, "byte_end": 10}),
                "byte_start (10) must be less than byte_end (10)",
            ),
            (
                serde_json::json!({"line_start": 0}),
                "line numbers are 1-based; line_start must be >= 1",
            ),
            (
                serde_json::json!({"line_start": 5, "line_end": 3}),
                "line_start must be <= line_end + 1 (set line_end = line_start - 1 for insert)",
            ),
        ];
        for (range, message) in cases {
            assert_eq!(parse(range.clone()).unwrap_err(), message, "{range}");
        }
    }

    #[test]
    fn the_published_schema_is_the_four_flat_range_fields() {
        let schema = serde_json::Value::Object(
            rmcp::handler::server::common::schema_for_type::<ReadArgs>()
                .as_ref()
                .clone(),
        );
        let properties = schema["properties"].as_object().expect("properties");
        for field in ["line_start", "line_end", "byte_start", "byte_end"] {
            assert!(properties.contains_key(field), "{field} missing: {schema}");
        }
        assert!(!properties.contains_key("span"), "{schema}");
        assert!(schema.get("description").is_none(), "{schema}");
        for combinator in ["anyOf", "oneOf", "allOf"] {
            assert!(schema.get(combinator).is_none(), "{combinator}: {schema}");
        }
    }
}
