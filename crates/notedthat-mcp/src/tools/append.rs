use crate::client::NotedThatClient;
use crate::error::McpToolError;
use rmcp::{ErrorData as McpError, model::CallToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppendArgs {
    pub kb: String,
    pub path: String,
    pub content: String,
    pub if_match: Option<String>,
}

pub(super) async fn run(
    _client: &NotedThatClient,
    args: AppendArgs,
) -> Result<CallToolResult, McpError> {
    let _ = (&args.kb, &args.path, &args.content, &args.if_match);
    Err(McpToolError::InvalidRequest("append tool implementation lands in T15".into()).into())
}
