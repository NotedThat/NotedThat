//! `GET /api/v1/knowledgebases/{kb_slug}/index`: one knowledge base's
//! search-index health (#97).
//!
//! Aggregate state only — never job ids, queue contents or document bytes
//! (D4, D38). The record is what the write paths, the worker and the `fs`
//! bridge stamped on [`notedthat_indexer::IndexHealth`]; the one live fact
//! added here is whether the queue is full at this moment.

use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use notedthat_core::{Verb, unix_to_rfc3339};
use notedthat_indexer::{IndexFailure, IndexState, KbHealthSnapshot, ReconcileSummary};
use serde::Serialize;

use super::openapi::{Forbidden, KbPath, NotFound, Unauthorized};

/// One knowledge base's search-index health.
#[derive(Serialize, utoipa::ToSchema)]
struct IndexHealthResponse {
    /// The knowledge base.
    kb_slug: String,
    /// The worst condition that holds, from `healthy` up to `failed`.
    state: IndexStateName,
    /// Index jobs queued for this knowledge base and not yet taken by the worker.
    pending: usize,
    /// The indexing queue, which every knowledge base shares.
    queue: QueueView,
    /// Whether the indexing worker is running.
    worker: WorkerState,
    /// When the worker last completed a job for this knowledge base, RFC 3339;
    /// `null` if never.
    last_indexed_at: Option<String>,
    /// The most recent indexing failure; `null` if none.
    last_failure: Option<FailureView>,
    /// The most recent reconciliation pass; `null` if none has run.
    last_reconcile: Option<ReconcileView>,
}

/// The index's state, most severe first. `failed`: the most recent outcome was
/// a failure, or the worker is gone. `stale`: changes may have gone unobserved,
/// and the pass that repairs that has not completed. `backpressured`: writes
/// were refused `503` recently, or the queue is full now. `indexing`: work is
/// queued or in progress. `healthy`: nothing pending, nothing failed since the
/// last success, nothing lost.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum IndexStateName {
    Healthy,
    Indexing,
    Backpressured,
    Stale,
    Failed,
}

/// Whether the indexing worker is running.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum WorkerState {
    Running,
    Stopped,
}

impl From<IndexState> for IndexStateName {
    fn from(state: IndexState) -> Self {
        match state {
            IndexState::Healthy => Self::Healthy,
            IndexState::Indexing => Self::Indexing,
            IndexState::Backpressured => Self::Backpressured,
            IndexState::Stale => Self::Stale,
            IndexState::Failed => Self::Failed,
        }
    }
}

/// The shared indexing queue at this moment.
#[derive(Serialize, utoipa::ToSchema)]
struct QueueView {
    /// Jobs waiting.
    depth: usize,
    /// Jobs the queue holds; a write finding it full is answered `503`.
    capacity: usize,
}

/// The most recent indexing failure.
#[derive(Serialize, utoipa::ToSchema)]
struct FailureView {
    /// When, RFC 3339.
    at: String,
    /// The object that failed; absent unless the caller may list it.
    #[serde(skip_serializing_if = "Option::is_none")]
    object_key: Option<String>,
    /// The pipeline's own error, first line; absent unless the caller's `list`
    /// grant spans the whole knowledge base.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

/// The most recent reconciliation pass.
#[derive(Serialize, utoipa::ToSchema)]
struct ReconcileView {
    /// When, RFC 3339.
    at: String,
    /// The prefix the pass walked; absent when it walked the whole base.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    // The four counts are shown or withheld together, by one visibility check
    // in `reconcile_view`. Separate fields rather than a flattened
    // `Option<struct>`, which has no faithful JSON Schema.
    /// Objects the pass found in storage. Present with the other counts, only
    /// when the caller may list what the pass walked.
    #[serde(skip_serializing_if = "Option::is_none")]
    objects_on_disk: Option<usize>,
    /// Objects already indexed from exactly these bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    unchanged: Option<usize>,
    /// Objects new to the index, or indexed from different bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<usize>,
    /// Keys the index holds that storage no longer has.
    #[serde(skip_serializing_if = "Option::is_none")]
    orphaned: Option<usize>,
}

/// Report one knowledge base's search-index health.
///
/// Answered with `Cache-Control: no-store`. Reachable by whoever would see the
/// knowledge base listed.
#[utoipa::path(
    get,
    path = "/knowledgebases/{kb_slug}/index",
    tag = "index",
    params(KbPath),
    security(("bearer" = []), ()),
    responses(
        (status = 200, description = "The index's health.", body = IndexHealthResponse,
            headers(("Cache-Control" = String, description = "`no-store`"))),
        (status = 401, response = Unauthorized),
        (status = 403, response = Forbidden),
        (status = 404, response = NotFound),
    ),
)]
pub(super) async fn get_index_health(
    State(state): State<AppState>,
    Path(kb_slug): Path<String>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    // The view is about the knowledge base, not about a key, so it follows the
    // listing rule: whoever would see the base listed may ask how its index is.
    access.require_visible().map_err(&err)?;

    let snapshot = state.index_health.snapshot(access.kb().as_str());
    let queue = QueueView {
        capacity: state.indexer_tx.max_capacity(),
        depth: state
            .indexer_tx
            .max_capacity()
            .saturating_sub(state.indexer_tx.capacity()),
    };
    let body = render(&access, &kb_slug, snapshot, queue);

    Ok(([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response())
}

fn render(
    access: &KbAccess,
    kb_slug: &str,
    snapshot: KbHealthSnapshot,
    queue: QueueView,
) -> IndexHealthResponse {
    // The queue is shared, so a full queue backpressures every knowledge base
    // right now, whether or not one of its own writes was the one refused.
    let state = if queue.depth >= queue.capacity
        && rank(snapshot.state) < rank(IndexState::Backpressured)
    {
        IndexState::Backpressured
    } else {
        snapshot.state
    };
    IndexHealthResponse {
        kb_slug: kb_slug.to_string(),
        state: state.into(),
        pending: snapshot.pending,
        queue,
        worker: if snapshot.worker_alive {
            WorkerState::Running
        } else {
            WorkerState::Stopped
        },
        last_indexed_at: snapshot.last_indexed_at.map(unix_to_rfc3339),
        last_failure: snapshot
            .last_failure
            .map(|failure| failure_view(access, failure)),
        last_reconcile: snapshot
            .last_reconcile
            .map(|summary| reconcile_view(access, summary)),
    }
}

/// How far down the precedence a state sits: `Healthy` lowest, `Failed`
/// highest. A live condition may only raise the state, never lower it.
fn rank(state: IndexState) -> u8 {
    match state {
        IndexState::Healthy => 0,
        IndexState::Indexing => 1,
        IndexState::Backpressured => 2,
        IndexState::Stale => 3,
        IndexState::Failed => 4,
    }
}

/// The failure, with two fields held back by who is asking.
///
/// The object key is shown only to a caller who could list it, so a caller
/// granted `read` under one prefix learns nothing about keys under another.
/// The summary — the pipeline's own first line — names the embedder or vector
/// store endpoint it could not reach, and as often the key it was working on
/// (`storage.head_object failed: object not found: internal/…`), so it can
/// carry exactly what the key gate holds back. It goes only to a caller whose
/// `list` grant spans the whole knowledge base, the bar the reconcile counts
/// use: an operator, or an agent holding such a grant, is who acts on it.
/// Everyone the listing rule admits still learns that indexing failed, and
/// when.
fn failure_view(access: &KbAccess, failure: IndexFailure) -> FailureView {
    let object_key = access
        .allows(Verb::List, &failure.object_key)
        .then_some(failure.object_key);
    let summary = access
        .filter(Verb::List)
        .covers_whole_kb()
        .then_some(failure.summary);
    FailureView {
        at: unix_to_rfc3339(failure.at),
        object_key,
        summary,
    }
}

/// The pass's time is for everyone the listing rule admits — it is what a
/// public caller needs to judge freshness. What it counted, and over what,
/// follow the `list` grant as one unit: a whole-base pass describes every key,
/// so its counts go only to a caller who may `list` every key; a pass over one
/// prefix names that prefix in `scope`, so it goes only to a caller who may
/// `list` the prefix — and its counts go with it, since "a pass over *some*
/// prefix found 3 changed" is still a signal about the part of the base the
/// caller cannot see. A caller granted `public/**` alone thus learns about
/// passes over `public/` and nothing about the rest.
///
/// A prefix is judged as a key: `internal/` matches `**` and `internal/**`,
/// not `public/**` nor `internal/a/**` — a narrower grant hides it, which errs
/// on the side of showing less.
fn reconcile_view(access: &KbAccess, summary: ReconcileSummary) -> ReconcileView {
    let visible = match &summary.scope {
        Some(prefix) => access.allows(Verb::List, prefix),
        None => access.filter(Verb::List).covers_whole_kb(),
    };
    let counted = |count: usize| visible.then_some(count);
    ReconcileView {
        at: unix_to_rfc3339(summary.at),
        scope: summary.scope.filter(|_| visible),
        objects_on_disk: counted(summary.objects_on_disk),
        unchanged: counted(summary.unchanged),
        changed: counted(summary.changed),
        orphaned: counted(summary.orphaned),
    }
}
