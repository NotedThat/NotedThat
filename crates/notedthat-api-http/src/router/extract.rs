//! Request-scoped extractors: the knowledge base, key, principal and policy a
//! handler needs, resolved and authorized before the handler sees the body.

use super::helpers::{event_source, parse_path};
use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap};
use notedthat_core::{ConditionalHeaders, Error as CoreError, EventSource, ObjectPath, Verb};
use std::marker::PhantomData;

/// The verb an object route needs on its key.
pub(crate) trait Requires {
    const VERB: Verb;
}
pub(crate) struct Read;
pub(crate) struct Write;
pub(crate) struct Delete;
impl Requires for Read {
    const VERB: Verb = Verb::Read;
}
impl Requires for Write {
    const VERB: Verb = Verb::Write;
}
impl Requires for Delete {
    const VERB: Verb = Verb::Delete;
}

/// An object route's request, authorized for `V` on its key. Rejected before
/// the body is touched, so a denied write never spools bytes.
pub(crate) struct ObjectRequest<V> {
    pub path: ObjectPath,
    pub access: KbAccess,
    pub conditionals: ConditionalHeaders,
    pub source: EventSource,
    pub request_id: String,
    _verb: PhantomData<V>,
}

impl<V: Requires> ObjectRequest<V> {
    /// The handler's error wrapper. Bind it as `let err = &req.err();` so the
    /// reference is `Copy` and `map_err(err)` can repeat.
    pub fn err(&self) -> impl Fn(ApiError) -> ApiErrorResponse + use<V> {
        wrap(self.request_id.clone())
    }

    /// Shared with the `replace/` dispatcher, which rewrites the raw key first.
    pub fn resolve(
        state: &AppState,
        headers: &HeaderMap,
        extensions: &Extensions,
        kb_slug: &str,
        raw_path: &str,
    ) -> Result<Self, ApiErrorResponse> {
        let request_id = extract_request_id(extensions);
        let err = wrap(request_id.clone());
        let path = parse_path(raw_path).map_err(&err)?;
        let access = KbAccess::resolve(state, kb_slug, extensions).map_err(&err)?;
        // Authorize before any storage call, so a denial costs nothing and reads
        // identically whether or not the object exists.
        access.require(V::VERB, path.as_str()).map_err(&err)?;
        Ok(Self {
            path,
            access,
            conditionals: ConditionalHeaders::from_header_map(headers),
            source: event_source(headers),
            request_id,
            _verb: PhantomData,
        })
    }
}

impl<V: Requires + Send> FromRequestParts<AppState> for ObjectRequest<V> {
    type Rejection = ApiErrorResponse;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, ApiErrorResponse> {
        let Path((kb_slug, raw_path)) = path::<(String, String)>(parts, state).await?;
        Self::resolve(
            state,
            &parts.headers,
            &parts.extensions,
            &kb_slug,
            &raw_path,
        )
    }
}

/// A knowledge-base route's request. Carries no verb: each route names its own gate.
pub(crate) struct KbRequest {
    pub access: KbAccess,
    pub request_id: String,
}

impl KbRequest {
    /// The handler's error wrapper; see [`ObjectRequest::err`].
    pub fn err(&self) -> impl Fn(ApiError) -> ApiErrorResponse + use<> {
        wrap(self.request_id.clone())
    }
}

impl FromRequestParts<AppState> for KbRequest {
    type Rejection = ApiErrorResponse;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, ApiErrorResponse> {
        let Path(kb_slug) = path::<String>(parts, state).await?;
        let request_id = extract_request_id(&parts.extensions);
        let access = KbAccess::resolve(state, &kb_slug, &parts.extensions)
            .map_err(wrap(request_id.clone()))?;
        Ok(Self { access, request_id })
    }
}

fn wrap(request_id: String) -> impl Fn(ApiError) -> ApiErrorResponse {
    move |error| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    }
}

/// Axum's path rejection in the API's envelope, instead of its plain text.
async fn path<T: serde::de::DeserializeOwned + Send>(
    parts: &mut Parts,
    state: &AppState,
) -> Result<Path<T>, ApiErrorResponse> {
    Path::<T>::from_request_parts(parts, state)
        .await
        .map_err(|rejection| ApiErrorResponse {
            error: ApiError::Core(CoreError::InvalidInput {
                message: rejection.body_text(),
            }),
            request_id: extract_request_id(&parts.extensions),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::InMemoryStorage;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::routing::put;
    use bytes::Bytes;
    use notedthat_core::KbSlug;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::task::Poll;
    use tower::ServiceExt as _;

    async fn handler(_req: ObjectRequest<Write>, body: Request) -> StatusCode {
        to_bytes(body.into_body(), usize::MAX).await.unwrap();
        StatusCode::OK
    }

    fn router() -> Router {
        let kb = KbSlug::try_new("notes").unwrap();
        let storage = Arc::new(InMemoryStorage::with_kbs([&kb]));
        let state = AppState::for_tests(storage, [("notes".to_string(), kb)].into());
        Router::new()
            .route("/{kb_slug}/{*object_path}", put(handler))
            .with_state(state)
    }

    /// No auth layer here, so the principal is `Anyone`, who holds no grant.
    #[tokio::test]
    async fn a_denied_write_never_polls_the_body() {
        let body = Body::from_stream(futures::stream::poll_fn(
            |_| -> Poll<Option<Result<Bytes, Infallible>>> {
                panic!("body polled before authorization")
            },
        ));
        let request = Request::builder()
            .method("PUT")
            .uri("/notes/secret.md")
            .body(body)
            .unwrap();
        let response = router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_path_rejection_answers_the_error_envelope() {
        let request = Request::builder()
            .method("PUT")
            .uri("/notes/%ff.md")
            .body(Body::empty())
            .unwrap();
        let response = router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid_request");
        assert!(json["request_id"].as_str().is_some_and(|id| !id.is_empty()));
    }
}
