use axum::response::IntoResponse;
use dav_server::fs::FsResult;
use notedthat_core::Principal;
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectMeta};

use crate::{
    filesystem::{DavTarget, collect_propfind_objects, storage_error_to_fs},
    state::WebDavState,
};

#[derive(Clone)]
pub(crate) struct PropfindListing {
    kb: KbSlug,
    prefix: Option<String>,
    objects: Vec<ObjectMeta>,
}

impl PropfindListing {
    pub(crate) fn matches(&self, kb: &KbSlug, prefix: Option<&str>) -> bool {
        self.kb == *kb && self.prefix.as_deref() == prefix
    }

    pub(crate) fn objects(&self) -> &[ObjectMeta] {
        &self.objects
    }
}

pub(crate) async fn prepare_propfind_listing(
    state: &WebDavState,
    target: &DavTarget,
    principal: Principal,
) -> FsResult<Option<PropfindListing>> {
    match target {
        DavTarget::Root | DavTarget::NonDeclaredKb => Ok(None),
        DavTarget::KbRoot(kb) => collect_propfind_objects(state, kb, None, principal)
            .await
            .map(|objects| {
                Some(PropfindListing {
                    kb: kb.clone(),
                    prefix: None,
                    objects,
                })
            }),
        DavTarget::Object(kb, path) => match state
            .storage
            .head_object(kb, path, ConditionalHeaders::default())
            .await
        {
            Ok(_) => Ok(None),
            Err(err) if err.is_not_found() => {
                let prefix = format!("{}/", path.as_str());
                collect_propfind_objects(state, kb, Some(&prefix), principal)
                    .await
                    .map(|objects| {
                        Some(PropfindListing {
                            kb: kb.clone(),
                            prefix: Some(prefix),
                            objects,
                        })
                    })
            }
            Err(err) => Err(storage_error_to_fs(&err)),
        },
    }
}

pub(crate) enum PropfindDepth {
    Default,
    Zero,
    One,
    Infinity,
    Invalid,
}

pub(crate) fn parse_propfind_depth(req: &axum::extract::Request) -> PropfindDepth {
    let mut values = req.headers().get_all("depth").iter();
    let Some(value) = values.next() else {
        return PropfindDepth::Default;
    };
    if values.next().is_some() {
        return PropfindDepth::Invalid;
    }

    match value.as_bytes() {
        b"0" => PropfindDepth::Zero,
        b"1" => PropfindDepth::One,
        b"infinity" | b"Infinity" => PropfindDepth::Infinity,
        _ => PropfindDepth::Invalid,
    }
}

pub(crate) fn depth_infinity_response() -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )],
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:error xmlns:D="DAV:"><D:propfind-finite-depth/></D:error>"#,
    )
        .into_response()
}
