use crate::{
    client::NotedThatClient,
    error::{McpToolError, map_response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use percent_encoding::percent_decode_str;
use rmcp::{
    ErrorData as McpError,
    model::{ErrorCode, ReadResourceResult, ResourceContents},
};
use url::Url;

const OCTET_STREAM: &str = "application/octet-stream";

struct ParsedResourceUri {
    kb_slug: String,
    object_key: String,
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
    let url = client.v1_url(&["knowledgebases", &parsed.kb_slug, &parsed.object_key]);
    let resp = client
        .authorized(client.http.get(url))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let bytes = resp
        .bytes()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;
    let mime_type = detect_mime_type(&parsed.object_key);

    let contents = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some(mime_type.to_string()),
            text,
            meta: None,
        },
        Err(err) => ResourceContents::BlobResourceContents {
            uri: uri.to_string(),
            mime_type: Some(OCTET_STREAM.to_string()),
            blob: BASE64_STANDARD.encode(err.into_bytes()),
            meta: None,
        },
    };

    Ok(ReadResourceResult::new(vec![contents]))
}

fn parse_resource_uri(uri: &str) -> Result<ParsedResourceUri, McpError> {
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
    // and `NotedThatClient::v1_url` performs the single HTTP path encoding pass.
    let object_key = percent_decode_str(encoded_path)
        .decode_utf8()
        .map_err(|err| invalid_params(format!("resource object path is not valid UTF-8: {err}")))?
        .into_owned();

    if object_key.is_empty() {
        return Err(invalid_params("resource URI must include an object path"));
    }

    Ok(ParsedResourceUri {
        kb_slug: kb_slug.to_string(),
        object_key,
    })
}

fn invalid_params(message: impl Into<String>) -> McpError {
    McpError::new(ErrorCode::INVALID_PARAMS, message.into(), None)
}

fn detect_mime_type(object_key: &str) -> &'static str {
    let lower = object_key.to_ascii_lowercase();
    if lower.ends_with(".md") {
        "text/markdown"
    } else if lower.ends_with(".txt") {
        "text/plain"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        OCTET_STREAM
    }
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

    fn only_content(result: ReadResourceResult) -> ResourceContents {
        assert_eq!(result.contents.len(), 1);
        result.contents.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn note_md_utf8_returns_text_resource_contents() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/knowledgebases/kb/note.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Hello"))
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
            .and(path("/v1/knowledgebases/kb/image.png"))
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
            .and(path("/v1/knowledgebases/kb/data.bin"))
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
            .and(path("/v1/knowledgebases/nonexistent-kb/x"))
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
}
