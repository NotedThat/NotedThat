//! Liveness and readiness probes. Not auth-gated (see router build).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::state::AppState;

pub(super) async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

/// Ready unless a configured event log says it cannot take or serve events —
/// the one backend with a cheap, connection-state answer today (D39, D55).
pub(super) async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if let Some(events) = &state.events
        && !events.ready()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "events": events.backend_name(),
                "reason": "event backend not connected",
            })),
        );
    }
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}
