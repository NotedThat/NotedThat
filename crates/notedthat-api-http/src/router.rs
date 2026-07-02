//! Axum router builder — middleware pipeline only for T17; handlers added in T19.

use crate::middleware::auth_middleware;
use crate::state::AppState;
use axum::extract::Request;
use axum::middleware::from_fn_with_state;
use axum::Router;
use http::HeaderName;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

/// A [`MakeRequestId`] implementation that generates `UUIDv7` request IDs.
#[derive(Clone, Copy, Default)]
pub struct MakeRequestUuidV7;

impl MakeRequestId for MakeRequestUuidV7 {
    fn make_request_id<B>(&mut self, _req: &Request<B>) -> Option<RequestId> {
        let id = Uuid::now_v7().to_string();
        let hv = id.parse().ok()?;
        Some(RequestId::new(hv))
    }
}

/// Build the axum [`Router`] with middleware pipeline.
///
/// T17 lands the middleware stack only. T19 will add all route handlers.
pub fn build_router(state: AppState) -> Router {
    let request_id_header = HeaderName::from_static("x-request-id");
    Router::new()
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(request_id_header.clone(), MakeRequestUuidV7))
                .layer(PropagateRequestIdLayer::new(request_id_header))
                .layer(TraceLayer::new_for_http())
                .layer(from_fn_with_state(state.clone(), auth_middleware)),
        )
        .with_state(state)
}
