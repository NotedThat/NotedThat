//! RFC 9728 protected-resource metadata. Not auth-gated: it exists so a client
//! holding no token yet can find out where to obtain one.

use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// `GET /.well-known/oauth-protected-resource`.
///
/// Answers `404` when the deployment publishes no metadata, which is every
/// deployment without `NOTEDTHAT_OIDC_RESOURCE`: an empty document would
/// invite a client to try a flow that cannot succeed.
pub(super) async fn protected_resource_metadata(State(state): State<AppState>) -> Response {
    match state.authenticator.protected_resource() {
        Some(resource) => Json(resource.document()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use crate::router::build_router;
    use crate::state::AppState;
    use crate::testing::{InMemoryStorage, NoopSearcher};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use notedthat_core::{Authenticator, ProtectedResource};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    fn app(authenticator: Authenticator) -> axum::Router {
        let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1);
        build_router(AppState {
            storage: Arc::new(InMemoryStorage::default()),
            declared_kbs: Arc::new(BTreeMap::new()),
            access_policies: Arc::new(BTreeMap::new()),
            authenticator: Arc::new(authenticator),
            max_body_size: 1024,
            max_patchable_size: 1024,
            indexer_tx,
            searcher: Arc::new(NoopSearcher),
            events: None,
        })
    }

    async fn fetch(app: axum::Router) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .uri("/.well-known/oauth-protected-resource")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
    }

    #[tokio::test]
    async fn the_document_is_served_without_a_credential_when_published() {
        // Given
        let app = app(
            Authenticator::new("token").with_protected_resource(ProtectedResource {
                resource: "https://notes.example.com".into(),
                authorization_servers: vec!["https://auth.example.com".into()],
                metadata_url: "https://notes.example.com/.well-known/oauth-protected-resource"
                    .into(),
            }),
        );

        // When
        let response = fetch(app).await;

        // Then
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["resource"], "https://notes.example.com");
        assert_eq!(json["authorization_servers"][0], "https://auth.example.com");
        assert_eq!(json["bearer_methods_supported"][0], "header");
    }

    #[tokio::test]
    async fn the_route_is_404_when_nothing_is_published() {
        assert_eq!(
            fetch(app(Authenticator::new("token"))).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
