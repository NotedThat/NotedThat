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

#[derive(Serialize)]
struct IndexHealthResponse {
    kb_slug: String,
    state: &'static str,
    pending: usize,
    queue: QueueView,
    worker: &'static str,
    last_indexed_at: Option<String>,
    last_failure: Option<FailureView>,
    last_reconcile: Option<ReconcileView>,
}

#[derive(Serialize)]
struct QueueView {
    depth: usize,
    capacity: usize,
}

#[derive(Serialize)]
struct FailureView {
    at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

#[derive(Serialize)]
struct ReconcileView {
    at: String,
    /// The prefix the pass walked; absent when it walked the whole base.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    counts: Option<ReconcileCounts>,
}

#[derive(Serialize)]
struct ReconcileCounts {
    objects_on_disk: usize,
    unchanged: usize,
    changed: usize,
    orphaned: usize,
}

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
        state: state.as_str(),
        pending: snapshot.pending,
        queue,
        worker: if snapshot.worker_alive {
            "running"
        } else {
            "stopped"
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
/// The summary — the pipeline's own error, which names the embedder or vector
/// store endpoint it could not reach — is shown only to a caller who presented
/// a credential: it is what an operator or an agent holding a token acts on,
/// and not something a deployment describes to the public. An anonymous
/// caller still learns that indexing failed, and when.
fn failure_view(access: &KbAccess, failure: IndexFailure) -> FailureView {
    let object_key = access
        .allows(Verb::List, &failure.object_key)
        .then_some(failure.object_key);
    let summary = (!access.is_anonymous()).then_some(failure.summary);
    FailureView {
        at: unix_to_rfc3339(failure.at),
        object_key,
        summary,
    }
}

/// The pass's time is for everyone the listing rule admits — it is what a
/// public caller needs to judge freshness. Its counts describe every key in
/// the knowledge base, so they go only to a caller who may `list` every key;
/// a caller granted `public/**` alone would otherwise learn how much lies
/// outside `public/` and how much of it is moving.
fn reconcile_view(access: &KbAccess, summary: ReconcileSummary) -> ReconcileView {
    let counts = access
        .filter(Verb::List)
        .covers_whole_kb()
        .then_some(ReconcileCounts {
            objects_on_disk: summary.objects_on_disk,
            unchanged: summary.unchanged,
            changed: summary.changed,
            orphaned: summary.orphaned,
        });
    ReconcileView {
        at: unix_to_rfc3339(summary.at),
        scope: summary.scope,
        counts,
    }
}
