use crate::{
    client::NotedThatClient,
    error::{McpToolError, map_response},
    path::encode_object_path,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use notedthat_core::kb::ObjectMeta;
use rmcp::{
    ErrorData as McpError,
    model::{ErrorCode, ListResourcesResult, Resource},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct M8Cursor {
    kb_slug: String,
    backend_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListKbsResponse {
    knowledgebases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ListObjectsResponse {
    objects: Vec<ObjectMeta>,
    next_cursor: Option<String>,
}

pub async fn list_resources(
    client: &NotedThatClient,
    cursor: Option<String>,
) -> Result<ListResourcesResult, McpError> {
    let kbs = list_kbs(client).await?;
    let Some(position) = start_position(&kbs, cursor)? else {
        return Ok(ListResourcesResult::default());
    };

    let page = list_objects(
        client,
        &position.kb_slug,
        position.backend_cursor.as_deref(),
    )
    .await?;
    let resources = page
        .objects
        .into_iter()
        .map(|object| resource_for(&position.kb_slug, object.key))
        .collect();
    let next_cursor = match page.next_cursor {
        Some(backend_cursor) => Some(encode_cursor(&M8Cursor {
            kb_slug: position.kb_slug,
            backend_cursor: Some(backend_cursor),
        })?),
        None => next_kb_after(&kbs, &position.kb_slug)
            .map(|kb_slug| {
                encode_cursor(&M8Cursor {
                    kb_slug: kb_slug.to_string(),
                    backend_cursor: None,
                })
            })
            .transpose()?,
    };

    Ok(ListResourcesResult {
        meta: None,
        next_cursor,
        resources,
    })
}

async fn list_kbs(client: &NotedThatClient) -> Result<Vec<String>, McpError> {
    let url = client.v1_url(&["knowledgebases"]);
    let resp = client
        .authorized(client.http.get(url))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await?;
    let body: ListKbsResponse = resp.json().await.map_err(McpToolError::Transport)?;
    Ok(body.knowledgebases)
}

async fn list_objects(
    client: &NotedThatClient,
    kb_slug: &str,
    cursor: Option<&str>,
) -> Result<ListObjectsResponse, McpError> {
    let url = client.v1_url(&["knowledgebases", kb_slug]);
    let mut query = Vec::new();
    if let Some(cursor) = cursor {
        query.push(("cursor", cursor));
    }
    let resp = client
        .authorized(client.http.get(url).query(&query))
        .send()
        .await
        .map_err(McpToolError::Transport)?;
    let resp = map_response(resp).await?;
    resp.json()
        .await
        .map_err(McpToolError::Transport)
        .map_err(McpError::from)
}

fn start_position(kbs: &[String], cursor: Option<String>) -> Result<Option<M8Cursor>, McpError> {
    match cursor {
        Some(cursor) => {
            let decoded = decode_cursor(&cursor)?;
            if kbs.iter().any(|kb| kb == &decoded.kb_slug) {
                Ok(Some(decoded))
            } else {
                Err(invalid_cursor(
                    "cursor references an unknown knowledge base",
                ))
            }
        }
        None => Ok(kbs.first().map(|kb_slug| M8Cursor {
            kb_slug: kb_slug.clone(),
            backend_cursor: None,
        })),
    }
}

fn next_kb_after<'a>(kbs: &'a [String], kb_slug: &str) -> Option<&'a str> {
    kbs.iter()
        .position(|candidate| candidate == kb_slug)
        .and_then(|index| kbs.get(index + 1))
        .map(String::as_str)
}

fn resource_for(kb_slug: &str, object_key: String) -> Resource {
    Resource::new(
        format!(
            "notedthat://{}/{}",
            kb_slug,
            encode_object_path(&object_key)
        ),
        object_key,
    )
}

fn encode_cursor(cursor: &M8Cursor) -> Result<String, McpError> {
    let json = serde_json::to_vec(cursor).map_err(McpToolError::Serialization)?;
    Ok(BASE64.encode(json))
}

fn decode_cursor(cursor: &str) -> Result<M8Cursor, McpError> {
    let json = BASE64
        .decode(cursor)
        .map_err(|_| invalid_cursor("cursor is not valid base64"))?;
    serde_json::from_slice(&json).map_err(|_| invalid_cursor("cursor is not valid M8 JSON"))
}

fn invalid_cursor(message: &str) -> McpError {
    McpError::new(ErrorCode::INVALID_PARAMS, message.to_string(), None)
}

#[cfg(test)]
mod tests;
