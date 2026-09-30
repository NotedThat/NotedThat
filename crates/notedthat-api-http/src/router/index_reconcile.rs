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

use super::openapi::{Conflict, Forbidden, KbPath, NotFound, Unauthorized};

/// The `202` body: which knowledge base, and that the pass has started.
#[derive(Serialize, utoipa::ToSchema)]
struct ReconcileStarted<'a> {
    /// The knowledge base.
    kb_slug: &'a str,
    /// Always `started`.
    #[schema(value_type = String, example = "started")]
    status: &'static str,
}

/// Start a reconciliation pass over one knowledge base.
///
/// Compares storage against the search index and re-indexes what differs, in
/// the background; the result appears as `last_reconcile` in
/// `GET /knowledgebases/{kb_slug}/index`. Only the deployment's service token
/// may ask: any other credential is `403`. Available on the `s3` backend only;
/// elsewhere `404`.
#[utoipa::path(
    post,
    path = "/knowledgebases/{kb_slug}/index/reconcile",
    tag = "index",
    params(KbPath),
    security(("bearer" = [])),
    responses(
        (status = 202, description = "The pass has started.", body = ReconcileStarted,
            headers(("Cache-Control" = String, description = "`no-store`"))),
        (status = 401, response = Unauthorized),
        (status = 403, response = Forbidden),
        (status = 404, response = NotFound),
        (status = 409, response = Conflict),
    ),
)]
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
