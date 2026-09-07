//! Liveness and readiness probes. Not auth-gated (see router build).

use axum::Json;
use axum::response::IntoResponse;

pub(super) async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

pub(super) async fn readyz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}
