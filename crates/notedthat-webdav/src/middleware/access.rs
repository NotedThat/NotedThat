use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use notedthat_core::{Principal, Verb, extract_basic_from_header, verify_basic_credentials};
use tower_http::request_id::RequestId;

use crate::access::{policy_for, verbs_for_method};
use crate::{filesystem::DavTarget, state::WebDavState};

use super::path_validation::{parse_webdav_uri_path, validate_webdav_read_uri_path};

const AUTH_GUIDANCE: &str = "valid credentials are required; anonymous access is available only \
where the knowledge base's access rules grant it, and invalid credentials are not treated as \
anonymous.";

/// Which read methods a target permits, cached for `OPTIONS` to report.
#[derive(Clone, Copy)]
pub(crate) struct DavAllow {
    /// `GET` and `HEAD` are permitted somewhere at this target.
    pub(crate) content: bool,
    /// `PROPFIND` is permitted at this target.
    pub(crate) propfind: bool,
}

fn challenge(request_id: &str) -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, AUTH_GUIDANCE).into_response();
    response.headers_mut().insert(
        "www-authenticate",
        HeaderValue::from_static("Basic realm=\"NotedThat\""),
    );
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(request_id).unwrap_or(HeaderValue::from_static("unknown")),
    );
    response
}

fn forbidden(request_id: &str) -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        "the knowledge base's access rules do not grant this operation",
    )
        .into_response();
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(request_id).unwrap_or(HeaderValue::from_static("unknown")),
    );
    response
}

/// Whether `principal` may apply `verb` at `target`.
fn allows(state: &WebDavState, target: &DavTarget, principal: Principal, verb: Verb) -> bool {
    match target {
        // The root collection belongs to no knowledge base, so no rule can name
        // it. A principal may act on it when at least one declared base is
        // visible to them — the same question for every verb, since the root has
        // no key to scope a grant to. The PROPFIND interceptor rejects recursive
        // walks, so listing the root never implies entering anything.
        //
        // The credential holder always reaches it, even when nothing is
        // declared: an empty collection is the truthful answer to "what is
        // here", and refusing it would mean a deployment with no knowledge
        // bases rejected its own operator.
        DavTarget::Root => {
            principal == Principal::SignedIn
                || state
                    .declared_kbs
                    .values()
                    .any(|kb| policy_for(state, kb).visible_in_listing(principal))
        }
        // A knowledge base's own collection has no key of its own; what matters
        // is whether the principal may list anything inside it.
        DavTarget::KbRoot(kb) => policy_for(state, kb).grants_any(principal, verb),
        DavTarget::Object(kb, path) => policy_for(state, kb).allows(principal, verb, path.as_str()),
        DavTarget::NonDeclaredKb => false,
    }
}

fn extract_request_id(req: &Request) -> String {
    req.extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// Authenticate a `WebDAV` request and authorize it against the target's access rules.
///
/// Supplied credentials always take precedence over anonymous access, so a
/// malformed or incorrect Basic credential cannot silently downgrade to a public
/// view. What changed with D51 is what happens *after* a credential verifies:
/// the rules bind the credential holder too, so a valid credential is the start
/// of the authorization question rather than the end of it.
pub async fn basic_auth_middleware(
    State(state): State<WebDavState>,
    mut req: Request,
    next: Next,
) -> Response {
    let request_id = extract_request_id(&req);

    let mut auth_headers = req.headers().get_all("authorization").iter();
    let auth_header = auth_headers.next();
    if auth_headers.next().is_some() {
        return challenge(&request_id);
    }

    let principal = match auth_header {
        Some(header) => {
            let authorized = header
                .to_str()
                .ok()
                .and_then(extract_basic_from_header)
                .is_some_and(|(username, password)| {
                    verify_basic_credentials(
                        &username,
                        &password,
                        state.username.as_str(),
                        state.password.as_str(),
                    )
                });
            if !authorized {
                return challenge(&request_id);
            }
            Principal::SignedIn
        }
        None => Principal::Anyone,
    };

    let verbs = verbs_for_method(req.method().as_str());

    // Anonymous mutation is refused by the model itself, whatever the target, so
    // decide it before the URI is parsed. Otherwise a malformed path on an
    // unauthenticated write answers `400` and tells an unauthenticated caller
    // something about the path they sent.
    if principal == Principal::Anyone && verbs.iter().any(|verb| verb.is_mutating()) {
        return challenge(&request_id);
    }

    if validate_webdav_read_uri_path(req.uri().path(), &state.declared_kbs).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let target_path = req
        .uri()
        .path()
        .strip_suffix('/')
        .filter(|path| !path.is_empty())
        .unwrap_or(req.uri().path());
    let Ok(target) = parse_webdav_uri_path(target_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let allow = DavAllow {
        // `Allow` describes what is worth asking for, not merely what passes
        // authorization: the root is a collection with no bytes, so advertising
        // GET there would be a lie even for a caller entitled to reach it.
        content: !matches!(target, DavTarget::Root)
            && allows(&state, &target, principal, Verb::Read),
        propfind: allows(&state, &target, principal, Verb::List),
    };

    let authorized = if verbs.is_empty() {
        // OPTIONS, and anything the surface refuses further in. Answer it when
        // the target permits any read at all, so a client can discover what it
        // may do without first being told it may do nothing.
        allow.content || allow.propfind
    } else {
        verbs
            .iter()
            .all(|verb| allows(&state, &target, principal, *verb))
    };

    if !authorized {
        // An anonymous caller gets the Basic challenge, because credentials
        // might change the answer. A credentialed one gets 403, because theirs
        // will not — and a second prompt would just be a lie (D43).
        return match principal {
            Principal::Anyone => challenge(&request_id),
            Principal::SignedIn => forbidden(&request_id),
        };
    }

    req.extensions_mut().insert(principal);
    req.extensions_mut().insert(allow);
    next.run(req).await
}
