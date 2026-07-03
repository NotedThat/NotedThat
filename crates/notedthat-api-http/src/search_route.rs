//! Handler for `POST /v1/knowledgebases/{kb_slug}/search`.

use axum::{
    Json,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use notedthat_core::{Error as CoreError, KbSlug, search::SearchRequest};

use crate::{
    error::{ApiError, ApiErrorResponse},
    state::AppState,
};

/// Maximum request body size for the search endpoint (64 KiB).
///
/// Smaller than the global PUT limit — covers the max 8 KiB query plus a
/// reasonable filter payload.
pub const SEARCH_BODY_MAX_BYTES: usize = 64 * 1024;

/// Handle `POST /v1/knowledgebases/{kb_slug}/search`.
pub async fn search_kb(
    State(state): State<AppState>,
    Path(kb_slug_raw): Path<String>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = crate::middleware::extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    // Validate slug format before declaration lookup so malformed slugs return
    // 400 `invalid_request` instead of leaking as a 404.
    let kb_slug = KbSlug::try_new(kb_slug_raw).map_err(|e| err(ApiError::Core(e)))?;
    let kb = state.declared_kbs.get(kb_slug.as_str()).cloned().ok_or_else(|| {
        err(ApiError::Core(CoreError::NotFound {
            resource: format!("KB '{}' not declared", kb_slug.as_str()),
        }))
    })?;

    let (parts, body) = req.into_parts();
    let body_bytes: Bytes = axum::body::to_bytes(body, SEARCH_BODY_MAX_BYTES)
        .await
        .map_err(|_| {
            err(ApiError::Core(CoreError::PayloadTooLarge {
                size: SEARCH_BODY_MAX_BYTES as u64 + 1,
                limit: SEARCH_BODY_MAX_BYTES as u64,
            }))
        })?;

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if content_type.is_none_or(|value| !value.starts_with("application/json")) {
        return Err(err(ApiError::Core(CoreError::InvalidInput {
            message: "Content-Type must be application/json".into(),
        })));
    }

    let raw: SearchRequest = serde_json::from_slice(&body_bytes).map_err(|e| {
        err(ApiError::Core(CoreError::InvalidInput {
            message: format!("invalid request body: {e}"),
        }))
    })?;
    let validated = raw
        .validate()
        .map_err(|e| err(ApiError::Core(CoreError::from(e))))?;

    let response = state
        .searcher
        .search(&kb, validated)
        .await
        .map_err(|e| err(ApiError::Core(CoreError::from(e))))?;

    Ok((StatusCode::OK, Json(response)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, body::to_bytes, http::Request, routing::post};
    use std::{collections::BTreeMap, sync::Arc};
    use tower::util::ServiceExt;

    const KB: &str = "notes";

    fn app() -> Router {
        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), KbSlug::try_new(KB).unwrap());
        let (indexer_tx, _) = tokio::sync::mpsc::channel(1024);
        let state = AppState {
            storage: Arc::new(crate::testing::InMemoryStorage::default()),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new("token".to_string()),
            max_body_size: 16 * 1024 * 1024,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        };

        Router::new()
            .route("/v1/knowledgebases/{kb_slug}/search", post(search_kb))
            .with_state(state)
    }

    fn request(body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/v1/knowledgebases/{KB}/search"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .unwrap()
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), SEARCH_BODY_MAX_BYTES + 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn valid_request_returns_200() {
        let response = app()
            .oneshot(request(r#"{"query":"install cargo"}"#))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json, serde_json::json!({"hits": []}));
    }

    #[tokio::test]
    async fn missing_content_type_returns_400() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/knowledgebases/{KB}/search"))
                    .body(Body::from(r#"{"query":"install cargo"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = response_json(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(json["request_id"].is_string());
    }

    #[tokio::test]
    async fn empty_query_returns_400() {
        let response = app()
            .oneshot(request(r#"{"query":""}"#))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = response_json(response).await;
        assert_eq!(json["error"], "invalid_request");
        assert!(json["message"].as_str().unwrap().contains("query"));
    }

    #[tokio::test]
    async fn body_too_large_returns_413() {
        let body = serde_json::json!({"query": "x".repeat(SEARCH_BODY_MAX_BYTES + 1)}).to_string();
        let response = app().oneshot(request(body)).await.unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let json = response_json(response).await;
        assert_eq!(json["error"], "payload_too_large");
        assert!(json["request_id"].is_string());
    }
}
