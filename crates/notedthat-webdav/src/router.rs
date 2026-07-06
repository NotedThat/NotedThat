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
        basic_auth_middleware, intercept_lock_unlock, intercept_options,
        intercept_propfind_too_large, intercept_proppatch, intercept_read_methods,
        intercept_write_methods,
    },
    state::WebDavState,
};

/// Build the `WebDAV` axum router with dav-server fallback and guard middleware.
pub fn build_router(state: WebDavState) -> Router {
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(WebDavStorage::new(Arc::new(state.clone()))))
        .autoindex(false)
        .build_handler();

    let dav_service = move |req: axum::extract::Request| {
        let handler = dav_handler.clone();
        async move { handler.handle(req).await }
    };

    Router::new().fallback(dav_service).layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
            .layer(PropagateRequestIdLayer::x_request_id())
            .layer(from_fn_with_state(state.clone(), basic_auth_middleware))
            .layer(TraceLayer::new_for_http())
            .layer(from_fn_with_state(
                state.clone(),
                intercept_propfind_too_large,
            ))
            .layer(from_fn_with_state(state.clone(), intercept_read_methods))
            .layer(from_fn_with_state(state, intercept_write_methods))
            .layer(from_fn(intercept_options))
            .layer(from_fn(intercept_proppatch))
            .layer(from_fn(intercept_lock_unlock)),
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

    #[test]
    fn test_intercept_read_methods_slotted_between_propfind_and_write() {
        let source = include_str!("router.rs");
        let build_start = source
            .find("pub fn build_router")
            .expect("build_router must exist");
        let tests_start = source
            .find("#[cfg(test)]")
            .expect("test module marker must exist");
        assert!(
            build_start < tests_start,
            "build_router must appear before the test module"
        );
        let stack = &source[build_start..tests_start];
        let propfind_pos = stack
            .find("intercept_propfind_too_large")
            .expect("intercept_propfind_too_large must be wired inside build_router");
        let read_pos = stack
            .find("intercept_read_methods")
            .expect("intercept_read_methods must be wired inside build_router");
        let write_pos = stack
            .find("intercept_write_methods")
            .expect("intercept_write_methods must be wired inside build_router");
        assert!(
            propfind_pos < read_pos && read_pos < write_pos,
            "layer order must be propfind_too_large -> read_methods -> write_methods, got \
             propfind_pos={propfind_pos}, read_pos={read_pos}, write_pos={write_pos}"
        );
    }
}
