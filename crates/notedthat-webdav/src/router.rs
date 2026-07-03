//! Axum router wiring for the `WebDAV` surface.

use axum::{Router, middleware::from_fn, middleware::from_fn_with_state};
use dav_server::DavHandler;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

use crate::{
    filesystem::WebDavStorage,
    middleware::{
        basic_auth_middleware, intercept_lock_unlock, intercept_options, intercept_proppatch,
        intercept_write_methods,
    },
    state::WebDavState,
};

/// Build the `WebDAV` axum router with dav-server fallback and guard middleware.
pub fn build_router(state: WebDavState) -> Router {
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(WebDavStorage::new(Arc::new(state.clone()))))
        .autoindex(false)
        // NOTE: no LockSystem is registered; LOCK/UNLOCK stay disabled for v1.
        .build_handler();

    let dav_service = move |req: axum::extract::Request| {
        let handler = dav_handler.clone();
        async move { handler.handle(req).await }
    };

    Router::new().fallback(dav_service).layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(TraceLayer::new_for_http())
            .layer(from_fn_with_state(state.clone(), intercept_write_methods))
            .layer(from_fn(intercept_options))
            .layer(from_fn(intercept_proppatch))
            .layer(from_fn(intercept_lock_unlock))
            .layer(from_fn_with_state(state, basic_auth_middleware)),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_router_disables_autoindex() {
        let source = include_str!("router.rs");
        assert!(
            source.contains(".autoindex(false)"),
            "autoindex must be false"
        );
    }

    #[test]
    fn test_router_does_not_register_locksystem() {
        let source = include_str!("router.rs");
        let forbidden = format!(".{}(", "locksystem");
        assert!(
            !source.contains(&forbidden),
            "no lock system must be registered"
        );
    }

    #[test]
    fn test_router_does_not_use_default_body_limit() {
        let source = include_str!("router.rs");
        let forbidden = ["Default", "Body", "Limit"].concat();
        assert!(
            !source.contains(&forbidden),
            "no body limit layer on WebDAV router"
        );
    }
}
