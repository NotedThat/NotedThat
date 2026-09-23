//! `POST /api/v1/knowledgebases/{kb_slug}/index/reconcile` — compare one
//! knowledge base's storage against the search index now, and re-index what
//! differs (D67).
//!
//! An operator action, not a knowledge-base verb: only the deployment's own
//! service token may ask, whatever the manifests grant anyone else. The pass
//! runs in the background; the answer is `202` and the result appears in
//! `GET …/index` as `last_reconcile`, exactly as a startup pass does.

use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// The `202` body: which knowledge base, and that the pass has started.
#[derive(Serialize)]
struct ReconcileStarted<'a> {
    kb_slug: &'a str,
    status: &'static str,
}

pub(super) async fn post_index_reconcile(
    State(state): State<AppState>,
    Path(kb_slug): Path<String>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    // Resolve first, so an undeclared slug is `404` for everyone, then the
    // operator check — in that order, so a verified identity learns only that
    // the knowledge base is declared and that this is not its call. `resolve`
    // attaches the principal and the policy; it never consults
    // `visible_in_listing`, so the `403`-vs-`404` split is available to any
    // verified identity, including one the manifests grant nothing anywhere.
    // That is no more than `GET …/index` already tells such a caller: it
    // resolves first too and then answers `403` on `require_visible`.
    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    access.require_service_token().map_err(&err)?;

    let trigger = state
        .reconcile
        .as_ref()
        .ok_or(ApiError::ReconcileUnsupported)
        .map_err(&err)?;
    trigger
        .trigger(access.kb())
        .map_err(|_busy| err(ApiError::ReconcileInProgress))?;

    tracing::info!(
        target: "notedthat::reconcile",
        kb = %access.kb().as_str(),
        "reconciliation requested"
    );
    Ok((
        StatusCode::ACCEPTED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(ReconcileStarted {
            kb_slug: access.kb().as_str(),
            status: "started",
        }),
    )
        .into_response())
}
