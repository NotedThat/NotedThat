use crate::{
    client::NotedThatClient,
    error::{McpToolError, map_response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use notedthat_core::{KbSlug, ObjectPath};
use percent_encoding::percent_decode_str;
use rmcp::{
    ErrorData as McpError,
    model::{ErrorCode, ReadResourceResult, ResourceContents},
};
use url::Url;

const OCTET_STREAM: &str = "application/octet-stream";

pub(crate) struct ParsedResourceUri {
    pub(crate) kb_slug: String,
    pub(crate) object_key: String,
}

/// Read an MCP resource by `notedthat://<kb_slug>/<percent-encoded object_key>` URI.
///
/// Byte ranges belong to the MCP `read` tool, not `resources/read`; the MCP
/// `resources/read` method accepts only `{ uri }`.
///
/// # Errors
/// Returns an MCP protocol error for malformed URIs or downstream HTTP errors.
pub async fn read_resource(
    client: &NotedThatClient,
    uri: &str,
) -> Result<ReadResourceResult, McpError> {
    let parsed = parse_resource_uri(uri)?;
    let url = client.api_v1_url(&["knowledgebases", &parsed.kb_slug, &parsed.object_key]);
    let resp = client
        .authorized(client.http.get(url))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    // The API returns the object's stored type; the extension table is only
    // a guess for a response that carries no usable header. The API stores
    // `application/octet-stream` when the writer declared no type, so that
    // value says nothing and falls back too.
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|ct| {
            !ct.split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case(OCTET_STREAM))
        })
        .map(str::to_owned);
    let bytes = client
        .read_body_bounded(resp)
        .await
        .map_err(McpError::from)?;

    let contents = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some(
                content_type.unwrap_or_else(|| detect_mime_type(&parsed.object_key).to_owned()),
            ),
            text,
            meta: None,
        },
        Err(err) => ResourceContents::BlobResourceContents {
            uri: uri.to_string(),
            mime_type: Some(
                content_type
                    .map_or_else(|| OCTET_STREAM.to_owned(), |ct| without_utf8_charset(&ct)),
            ),
            blob: BASE64_STANDARD.encode(err.into_bytes()),
            meta: None,
        },
    };

    Ok(ReadResourceResult::new(vec![contents]))
}

pub(crate) fn parse_resource_uri(uri: &str) -> Result<ParsedResourceUri, McpError> {
    if uri.is_empty() {
        return Err(invalid_params("resource URI must not be empty"));
    }

    let parsed =
        Url::parse(uri).map_err(|err| invalid_params(format!("invalid resource URI: {err}")))?;
    if parsed.scheme() != "notedthat" {
        return Err(invalid_params("resource URI scheme must be notedthat"));
    }

    let Some(kb_slug) = parsed.host_str().filter(|slug| !slug.is_empty()) else {
        return Err(invalid_params(
            "resource URI must include a knowledge base slug",
        ));
    };

    let encoded_path = parsed.path().strip_prefix('/').unwrap_or(parsed.path());
    if encoded_path.is_empty() {
        return Err(invalid_params("resource URI must include an object path"));
    }

    // Apply percent-decoding EXACTLY ONCE: resource URIs carry one encoded object key,
    // and `NotedThatClient::api_v1_url` performs the single HTTP path encoding pass.
    let object_key = percent_decode_str(encoded_path)
        .decode_utf8()
        .map_err(|err| invalid_params(format!("resource object path is not valid UTF-8: {err}")))?
        .into_owned();

    if object_key.is_empty() {
        return Err(invalid_params("resource URI must include an object path"));
    }
    // The API's own rules, before the key reaches `api_v1_url`, whose
    // `push` would drop a `.` or `..` segment instead of encoding it.
    KbSlug::try_new(kb_slug)
        .map_err(|err| invalid_params(format!("invalid resource knowledge base: {err}")))?;
    ObjectPath::try_object_key(&object_key)
        .map_err(|err| invalid_params(format!("invalid resource object path: {err}")))?;

    Ok(ParsedResourceUri {
        kb_slug: kb_slug.to_string(),
        object_key,
    })
}

fn invalid_params(message: impl Into<String>) -> McpError {
    McpError::new(ErrorCode::INVALID_PARAMS, message.into(), None)
}

/// Drop a `charset=utf-8` parameter: these bytes just failed UTF-8 decoding,
/// so the charset the API added to a textual type would be wrong for them.
/// Any other declared charset may be right and stays.
fn without_utf8_charset(content_type: &str) -> String {
    // shortcut: splits on every `;`, so a quoted parameter value holding `;` is cut; parse per RFC 9110 if stored types ever carry one.
    let mut parts = content_type.split(';');
    let essence = parts.next().unwrap_or_default();
    // A stored type without `type/subtype` (such as a bare `charset=utf-8`) names nothing.
    if !essence.contains('/') {
        return OCTET_STREAM.to_owned();
    }
    let params = parts.filter(|part| {
        !part.split_once('=').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("charset")
                && ["utf-8", "utf8"]
                    .iter()
                    .any(|utf8| value.trim().trim_matches('"').eq_ignore_ascii_case(utf8))
        })
    });
    std::iter::once(essence)
        .chain(params)
        .collect::<Vec<_>>()
        .join(";")
}

fn detect_mime_type(object_key: &str) -> &'static str {
    notedthat_core::mime::from_key(object_key).unwrap_or(OCTET_STREAM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;

    /// The key's extension decides regardless of case, as it does everywhere
    /// else: `Notes.MD` is Markdown, not binary (#344).
    #[test]
    fn the_extension_names_the_type_in_any_case() {
        assert_eq!(detect_mime_type("notes/Notes.MD"), "text/markdown");
        assert_eq!(detect_mime_type("notes/Draft.markdown"), "text/markdown");
        assert_eq!(detect_mime_type("img/Photo.JPG"), "image/jpeg");
        assert_eq!(detect_mime_type("notes/README"), OCTET_STREAM);
    }
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    fn only_content(result: ReadResourceResult) -> ResourceContents {
        assert_eq!(result.contents.len(), 1);
        result.contents.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn note_md_utf8_returns_text_resource_contents() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/note.md"))
            // `set_body_bytes` sends no Content-Type, so this covers the
            // extension fallback.
            .respond_with(ResponseTemplate::new(200).set_body_bytes("# Hello"))
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/note.md")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                meta,
            } => {
                assert_eq!(uri, "notedthat://kb/note.md");
                assert_eq!(mime_type.as_deref(), Some("text/markdown"));
                assert_eq!(text, "# Hello");
                assert!(meta.is_none());
            }
            other => panic!("expected text contents, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_png_non_utf8_returns_blob_resource_contents() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/image.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0x89, b'P', b'N', b'G']))
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/image.png")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                meta,
            } => {
                assert_eq!(uri, "notedthat://kb/image.png");
                assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
                assert_eq!(blob, "iVBORw==");
                assert!(meta.is_none());
            }
            other => panic!("expected blob contents, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn data_bin_non_utf8_returns_octet_stream_blob() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/data.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0x00, 0xFF, 0x10]))
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/data.bin")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                meta,
            } => {
                assert_eq!(uri, "notedthat://kb/data.bin");
                assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
                assert_eq!(blob, "AP8Q");
                assert!(meta.is_none());
            }
            other => panic!("expected blob contents, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_api_content_type_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/config.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"a":1}"#, "application/json; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/config.json")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::TextResourceContents {
                mime_type, text, ..
            } => {
                assert_eq!(
                    mime_type.as_deref(),
                    Some("application/json; charset=utf-8")
                );
                assert_eq!(text, r#"{"a":1}"#);
            }
            other => panic!("expected text contents, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stored_octet_stream_falls_back_to_the_extension() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/todo.txt"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("buy milk", "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/todo.txt")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::TextResourceContents { mime_type, .. } => {
                assert_eq!(mime_type.as_deref(), Some("text/plain"));
            }
            other => panic!("expected text contents, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_binary_resource_reports_the_declared_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/image.png"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(vec![0x89, b'P', b'N', b'G'], "image/png"),
            )
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/image.png")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::BlobResourceContents { mime_type, .. } => {
                assert_eq!(mime_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected blob contents, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_binary_resource_drops_the_utf8_charset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/data.csv"))
            .respond_with(
                // Latin-1 `café`: not UTF-8, whatever the stored type claims.
                ResponseTemplate::new(200)
                    .set_body_raw(b"caf\xe9".to_vec(), "text/csv; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/data.csv")
            .await
            .unwrap();

        match only_content(result) {
            ResourceContents::BlobResourceContents { mime_type, .. } => {
                assert_eq!(mime_type.as_deref(), Some("text/csv"));
            }
            other => panic!("expected blob contents, got {other:?}"),
        }
    }

    #[test]
    fn only_a_utf8_charset_is_dropped() {
        assert_eq!(
            without_utf8_charset("text/csv; charset=\"UTF-8\""),
            "text/csv"
        );
        assert_eq!(
            without_utf8_charset("text/plain; charset=utf8"),
            "text/plain"
        );
        assert_eq!(
            without_utf8_charset("text/plain; charset=utf-16"),
            "text/plain; charset=utf-16"
        );
    }

    #[test]
    fn a_stored_type_without_a_media_type_is_octet_stream() {
        assert_eq!(without_utf8_charset("charset=utf-8"), OCTET_STREAM);
        assert_eq!(without_utf8_charset(""), OCTET_STREAM);
    }

    #[tokio::test]
    async fn malformed_uri_returns_error() {
        let server = MockServer::start().await;

        let result = read_resource(&client(&server.uri()), "notedthat://").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn foreign_uri_returns_error() {
        let server = MockServer::start().await;

        let result = read_resource(&client(&server.uri()), "foreign://kb/x").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nonexistent_kb_http_error_is_propagated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/nonexistent-kb/x"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": "not_found",
                "message": "KB not declared"
            })))
            .mount(&server)
            .await;

        let result = read_resource(&client(&server.uri()), "notedthat://nonexistent-kb/x").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn empty_path_returns_error() {
        let server = MockServer::start().await;

        let result = read_resource(&client(&server.uri()), "notedthat://kb/").await;

        assert!(result.is_err());
    }

    /// The budget is on the bytes fetched: a blob resource is ~4/3 of that on the
    /// wire, which is documented rather than a second knob.
    #[tokio::test]
    async fn an_object_within_the_budget_is_served_whole() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/big.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFFu8; 1024]))
            .mount(&server)
            .await;
        let c = client(&server.uri()).with_max_read_bytes(1024);
        let result = read_resource(&c, "notedthat://kb/big.bin").await.unwrap();
        match only_content(result) {
            ResourceContents::BlobResourceContents { blob, .. } => {
                assert_eq!(blob.len(), 1368, "base64 of 1024 bytes");
            }
            other => panic!("expected a blob, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_object_is_refused_and_points_at_the_read_tool() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/kb/big.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFFu8; 1025]))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server.uri()).with_max_read_bytes(1024);
        let error = read_resource(&c, "notedthat://kb/big.bin")
            .await
            .unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            error
                .message
                .starts_with("response_too_large: the object is 1025 bytes"),
            "{}",
            error.message
        );
        assert!(error.message.contains("read tool"), "{}", error.message);
        server.verify().await;
    }

    #[test]
    fn unaddressable_resource_uri_is_invalid_params() {
        for uri in [
            "notedthat://notes/index",
            "notedthat://notes/..",
            "notedthat://notes/a%2F..%2Fb.md",
            "notedthat://No_tes/a.md",
        ] {
            let Err(err) = parse_resource_uri(uri) else {
                panic!("{uri} parsed");
            };
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{uri}");
        }
    }
}
