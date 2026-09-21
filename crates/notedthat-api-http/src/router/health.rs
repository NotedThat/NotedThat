//! Liveness and readiness probes. Not auth-gated (see router build).
//!
//! `/readyz` covers the storage backend, the vector store and a configured
//! event broker (D39, D55, D64). It does not cover the `fs` watcher (a lost
//! watch is logged, D50), the embedder, or how fresh the index is — that is
//! per knowledge base, at `GET /api/v1/knowledgebases/{kb}/index` (D62).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::state::AppState;

pub(super) async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

/// Ready when every backend's latest check is ok.
///
/// Storage and search come from the snapshot the server's poller publishes;
/// this handler never probes a backend itself, so a probe storm costs the
/// backends nothing. The event log is the one backend with a cheap
/// connection-state answer, read inline as it always was.
pub(super) async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.readiness.borrow().clone();
    let mut ready = snapshot.is_ready();
    let mut checks = serde_json::Map::new();
    checks.insert("storage".to_string(), snapshot.storage.to_json());
    checks.insert("search".to_string(), snapshot.search.to_json());
    if let Some(events) = &state.events {
        let connected = events.ready();
        ready &= connected;
        let check = if connected {
            serde_json::json!({ "backend": events.backend_name(), "status": "ok" })
        } else {
            serde_json::json!({
                "backend": events.backend_name(),
                "status": "unavailable",
                "reason": "disconnected",
            })
        };
        checks.insert("events".to_string(), check);
    }
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    // The top-level word mirrors the worst check, so a reader keying on it
    // alone still learns of a `degraded` check; only an outage costs the `200`.
    let word = if !ready {
        "unavailable"
    } else if snapshot.is_degraded() {
        "degraded"
    } else {
        "ok"
    };
    (
        status,
        Json(serde_json::json!({
            "status": word,
            "checks": checks,
        })),
    )
}
