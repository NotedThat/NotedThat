use super::range::{EditSpan, RangeArgs};
use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// The tool's arguments, parsed: the range is one `EditSpan`, so a call naming
// no range, half a pair or both pairs is refused while its arguments are
// deserialized and never reaches `run`. The published schema is
// `RawEditArgs`'s four flat range fields. Plain comments, not rustdoc: a doc
// here would become the input schema's description.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(try_from = "RawEditArgs")]
#[schemars(with = "RawEditArgs")]
pub struct EditArgs {
    pub kb: String,
    pub path: String,
    pub span: EditSpan,
    pub content: String,
    pub if_match: String,
}

// The tool's arguments as a client sends them. Named `EditArgs` in the
// schema, so the published input schema keeps the title it had before the
// raw twin existed.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(rename = "EditArgs")]
pub struct RawEditArgs {
    /// Knowledge base slug.
    pub kb: String,
    /// Object path within the knowledge base.
    pub path: String,
    /// First line to replace (1-based inclusive). Requires `line_end`; not with `byte_*`.
    pub line_start: Option<u64>,
    /// Last line to replace (1-based inclusive). Requires `line_start`. Set to `line_start - 1` for an insert point.
    pub line_end: Option<u64>,
    /// First byte to replace (0-based inclusive). Requires `byte_end`; not with `line_*`.
    pub byte_start: Option<u64>,
    /// End byte to replace (0-based exclusive). Requires `byte_start`, and must exceed it: a zero-width byte range is not supported.
    pub byte_end: Option<u64>,
    /// Replacement content.
    pub content: String,
    /// Required `ETag` from a previous GET or write (concurrency control).
    pub if_match: String,
}

impl TryFrom<RawEditArgs> for EditArgs {
    type Error = String;

    fn try_from(raw: RawEditArgs) -> Result<Self, Self::Error> {
        let span = EditSpan::parse(RangeArgs {
            line_start: raw.line_start,
            line_end: raw.line_end,
            byte_start: raw.byte_start,
            byte_end: raw.byte_end,
        })?;
        Ok(Self {
            kb: raw.kb,
            path: raw.path,
            span,
            content: raw.content,
            if_match: raw.if_match,
        })
    }
}

#[derive(Debug, Serialize)]
struct EditResult {
    etag: Option<String>,
    location: Option<String>,
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: EditArgs,
) -> Result<CallToolResult, McpError> {
    let kb_enc = encode_kb_slug(&args.kb);
    let url = client.api_v1_url(&["knowledgebases", &kb_enc, &args.path]);

    let req = client
        .authorized(client.http.patch(url))
        .header("Content-Range", args.span.content_range())
        .header("If-Match", &args.if_match)
        .body(args.content.into_bytes());

    let resp = req.send().await.map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;

    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let result = EditResult { etag, location };
    Ok(CallToolResult::success(vec![ContentBlock::json(result)?]))
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
    fn parse(range: serde_json::Value) -> Result<EditArgs, String> {
        let mut value = serde_json::json!({
            "kb": "notes",
            "path": "hello.md",
            "content": "replacement",
            "if_match": "\"abc\"",
        });
        let serde_json::Value::Object(range) = range else {
            panic!("a range is an object: {range}");
        };
        value.as_object_mut().unwrap().extend(range);
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn edit_args(line_start: u64, line_end: u64) -> EditArgs {
        parse(serde_json::json!({"line_start": line_start, "line_end": line_end})).unwrap()
    }

    fn byte_edit_args(byte_start: u64, byte_end: u64, content: &str) -> EditArgs {
        EditArgs {
            content: content.into(),
            if_match: "\"e\"".into(),
            ..parse(serde_json::json!({"byte_start": byte_start, "byte_end": byte_end})).unwrap()
        }
    }

    #[tokio::test]
    async fn sends_patch_with_line_range_and_if_match_headers() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "lines 2-3/*"))
            .and(header("If-Match", "\"abc\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(2, 3)).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn ok_response_returns_etag() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"next\"")
                    .insert_header("Location", "/api/v1/knowledgebases/notes/hello.md"),
            )
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(1, 1)).await.unwrap();
        let rendered = format!("{result:?}");

        assert!(rendered.contains("next"), "etag missing: {rendered}");
    }

    #[tokio::test]
    async fn precondition_failed_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({
                "error": "precondition_failed",
                "message": "etag mismatch"
            })))
            .mount(&server)
            .await;

        let c = client(&server.uri());

        assert!(run(&c, edit_args(1, 1)).await.is_err());
    }

    #[tokio::test]
    async fn insert_point_encoding_sends_reversed_adjacent_line_range() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "lines 5-4/*"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, edit_args(5, 4)).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn byte_mode_happy_path_sends_content_range_bytes_100_199() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "bytes 100-199/*"))
            .and(header("If-Match", "\"e\""))
            .and(wiremock::matchers::body_string("…"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, byte_edit_args(100, 200, "…")).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn byte_mode_delete_empty_body() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "bytes 100-199/*"))
            .and(wiremock::matchers::body_string(""))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, byte_edit_args(100, 200, "")).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn byte_end_off_by_one_conversion_regression() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/knowledgebases/notes/hello.md"))
            .and(header("Content-Range", "bytes 0-9/*"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server.uri());
        let result = run(&c, byte_edit_args(0, 10, "x")).await.unwrap();

        assert!(!result.content.is_empty());
        server.verify().await;
    }

    #[test]
    fn a_range_that_breaks_a_rule_is_refused_while_parsing_with_that_rule() {
        let cases = [
            (
                serde_json::json!({"line_start": 1, "line_end": 10, "byte_start": 100, "byte_end": 200}),
                "line_* and byte_* arguments are mutually exclusive; provide one pair or the other",
            ),
            (
                serde_json::json!({}),
                "edit requires either (line_start, line_end) or (byte_start, byte_end); use append for EOF-only writes",
            ),
            (
                serde_json::json!({"line_start": 1}),
                "line_start requires line_end; provide both or omit both",
            ),
            (
                serde_json::json!({"line_end": 1}),
                "line_end requires line_start; provide both or omit both",
            ),
            (
                serde_json::json!({"byte_start": 100}),
                "byte_start requires byte_end; provide both or omit both",
            ),
            (
                serde_json::json!({"byte_end": 200}),
                "byte_end requires byte_start; provide both or omit both",
            ),
            (
                serde_json::json!({"line_start": 0, "line_end": 0}),
                "line numbers are 1-based; line_start must be >= 1",
            ),
            (
                serde_json::json!({"line_start": 5, "line_end": 3}),
                "line_start must be <= line_end + 1 (set line_end = line_start - 1 for insert)",
            ),
            (
                serde_json::json!({"byte_start": 100, "byte_end": 100}),
                "byte_start must be strictly less than byte_end; byte-mode insert (zero-width range) is not supported in v1 — the PATCH byte-range wire contract cannot represent it",
            ),
            (
                serde_json::json!({"byte_start": 200, "byte_end": 100}),
                "byte_start must be strictly less than byte_end; byte-mode insert (zero-width range) is not supported in v1 — the PATCH byte-range wire contract cannot represent it",
            ),
        ];
        for (range, message) in cases {
            assert_eq!(parse(range.clone()).unwrap_err(), message, "{range}");
        }
    }

    #[test]
    fn the_published_schema_is_the_four_flat_range_fields() {
        let schema = serde_json::Value::Object(
            rmcp::handler::server::common::schema_for_type::<EditArgs>()
                .as_ref()
                .clone(),
        );
        let properties = schema["properties"].as_object().expect("properties");
        for field in ["line_start", "line_end", "byte_start", "byte_end"] {
            assert!(properties.contains_key(field), "{field} missing: {schema}");
        }
        assert!(!properties.contains_key("span"), "{schema}");
        assert_eq!(schema["title"], "EditArgs", "{schema}");
        assert!(schema.get("description").is_none(), "{schema}");
        for combinator in ["anyOf", "oneOf", "allOf"] {
            assert!(schema.get(combinator).is_none(), "{combinator}: {schema}");
        }
    }
}
