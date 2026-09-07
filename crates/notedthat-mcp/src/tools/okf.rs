//! MCP tools for Open Knowledge Format (OKF) v0.2 (D48).
//!
//! Each one is a thin client over the corresponding `/v1/okf/…` route, matching
//! how every other tool in this crate is built.

use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Arguments for `okf_validate`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateArgs {
    pub kb: String,
    /// Validate exactly this object. Mutually exclusive with `prefix`.
    pub path: Option<String>,
    /// Restrict a bundle walk to this key prefix.
    pub prefix: Option<String>,
    /// Objects to inspect in this page.
    pub limit: Option<u32>,
    /// Continuation cursor from a previous report.
    pub cursor: Option<String>,
    /// Check that in-body links resolve. Off by default: it costs a round trip
    /// per unseen target.
    pub check_links: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ValidateBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    check_links: bool,
}

/// Run `okf_validate`.
pub(super) async fn validate(
    client: &NotedThatClient,
    args: ValidateArgs,
) -> Result<CallToolResult, McpError> {
    let url = client.v1_url(&["okf", &encode_kb_slug(&args.kb), "validate"]);
    let body = ValidateBody {
        path: args.path,
        prefix: args.prefix,
        limit: args.limit,
        cursor: args.cursor,
        check_links: args.check_links.unwrap_or(false),
    };
    json_result(client.authorized(client.http.post(url).json(&body))).await
}

/// Arguments for `okf_browse`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseArgs {
    pub kb: String,
    /// Directory to browse. Defaults to the knowledge base root.
    pub dir: Option<String>,
}

/// Run `okf_browse`.
pub(super) async fn browse(
    client: &NotedThatClient,
    args: BrowseArgs,
) -> Result<CallToolResult, McpError> {
    let url = client.v1_url(&["okf", &encode_kb_slug(&args.kb), "index"]);
    let query: Vec<(&str, String)> = args.dir.into_iter().map(|dir| ("dir", dir)).collect();
    json_result(client.authorized(client.http.get(url).query(&query))).await
}

/// Arguments for `okf_computation`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComputationArgs {
    pub kb: String,
    /// The concept to read the contract from.
    pub path: String,
}

/// Run `okf_computation`.
pub(super) async fn computation(
    client: &NotedThatClient,
    args: ComputationArgs,
) -> Result<CallToolResult, McpError> {
    let url = client.v1_url(&["okf", &encode_kb_slug(&args.kb), "computation"]);
    json_result(client.authorized(client.http.get(url).query(&[("path", &args.path)]))).await
}

/// Arguments for `okf_reindex_directory`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReindexArgs {
    pub kb: String,
    /// Directory to rebuild. Defaults to the knowledge base root.
    pub dir: Option<String>,
    /// Set to `false` to write the result. Defaults to `true`.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ReindexBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    dry_run: bool,
}

/// Run `okf_reindex_directory`.
pub(super) async fn reindex(
    client: &NotedThatClient,
    args: ReindexArgs,
) -> Result<CallToolResult, McpError> {
    let url = client.v1_url(&["okf", &encode_kb_slug(&args.kb), "reindex"]);
    let body = ReindexBody {
        dir: args.dir,
        // Default to a dry run: this tool can rewrite a file the user authored.
        dry_run: args.dry_run.unwrap_or(true),
    };
    json_result(client.authorized(client.http.post(url).json(&body))).await
}

async fn json_result(request: reqwest::RequestBuilder) -> Result<CallToolResult, McpError> {
    let resp = request.send().await.map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await.map_err(McpError::from)?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)?;
    Ok(CallToolResult::success(vec![ContentBlock::json(body)?]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path, query_param},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    #[tokio::test]
    async fn validate_posts_to_the_okf_namespace() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/okf/notes/validate"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"conformant": true, "findings": []})),
            )
            .mount(&server)
            .await;
        let args = ValidateArgs {
            kb: "notes".into(),
            path: None,
            prefix: None,
            limit: None,
            cursor: None,
            check_links: None,
        };
        assert!(validate(&client(&server.uri()), args).await.is_ok());
    }

    #[tokio::test]
    async fn validate_defaults_link_checking_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/okf/notes/validate"))
            .and(body_string_contains("\"check_links\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let args = ValidateArgs {
            kb: "notes".into(),
            path: None,
            prefix: None,
            limit: None,
            cursor: None,
            check_links: None,
        };
        assert!(validate(&client(&server.uri()), args).await.is_ok());
    }

    #[tokio::test]
    async fn validate_surfaces_a_400_as_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/okf/notes/validate"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error":"invalid_request","message":"path and prefix"}),
            ))
            .mount(&server)
            .await;
        let args = ValidateArgs {
            kb: "notes".into(),
            path: Some("a.md".into()),
            prefix: Some("x/".into()),
            limit: None,
            cursor: None,
            check_links: None,
        };
        assert!(validate(&client(&server.uri()), args).await.is_err());
    }

    #[tokio::test]
    async fn browse_passes_the_directory_as_a_query_parameter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/okf/notes/index"))
            .and(query_param("dir", "tables"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let args = BrowseArgs {
            kb: "notes".into(),
            dir: Some("tables".into()),
        };
        assert!(browse(&client(&server.uri()), args).await.is_ok());
    }

    #[tokio::test]
    async fn computation_passes_the_concept_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/okf/notes/computation"))
            .and(query_param("path", "metrics/rev.md"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let args = ComputationArgs {
            kb: "notes".into(),
            path: "metrics/rev.md".into(),
        };
        assert!(computation(&client(&server.uri()), args).await.is_ok());
    }

    #[tokio::test]
    async fn computation_surfaces_a_rejected_absolute_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/okf/notes/computation"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error":"invalid_request","message":"absolute URLs are never fetched"}),
            ))
            .mount(&server)
            .await;
        let args = ComputationArgs {
            kb: "notes".into(),
            path: "a.md".into(),
        };
        assert!(computation(&client(&server.uri()), args).await.is_err());
    }

    #[tokio::test]
    async fn reindex_defaults_to_a_dry_run() {
        // This tool can rewrite a file the user authored, so the default matters.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/okf/notes/reindex"))
            .and(body_string_contains("\"dry_run\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let args = ReindexArgs {
            kb: "notes".into(),
            dir: Some("tables".into()),
            dry_run: None,
        };
        assert!(reindex(&client(&server.uri()), args).await.is_ok());
    }

    #[tokio::test]
    async fn reindex_writes_only_when_asked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/okf/notes/reindex"))
            .and(body_string_contains("\"dry_run\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let args = ReindexArgs {
            kb: "notes".into(),
            dir: None,
            dry_run: Some(false),
        };
        assert!(reindex(&client(&server.uri()), args).await.is_ok());
    }
}
