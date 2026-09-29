//! `PROPPATCH` (RFC 4918 §9.2): answered, never applied.
//!
//! `DAV: 1` makes `PROPPATCH` a method this server must implement (RFC 4918 §18.1), but
//! `NotedThat` stores no dead properties and every live one is computed from the object.
//! So each property in the request is refused with `403` and
//! `cannot-modify-protected-property` — except the Windows `Win32*` timestamps in the
//! `urn:schemas-microsoft-com:` namespace. Windows Explorer sets those after every upload
//! and reports the copy as failed when they are refused, so they answer `200` without
//! being stored, as dav-server does.
//!
//! RFC 4918 §9.2 makes the instructions atomic: when any property is refused, the rest
//! answer `424 Failed Dependency` rather than `200`.

use std::fmt::Write as _;

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use notedthat_core::{ConditionalHeaders, ObjectState, StorageError, evaluate_write_preconditions};

use super::super::path_validation::parse_webdav_uri_path;
use crate::filesystem::DavTarget;
use crate::if_header::IfHeader;
use crate::state::WebDavState;

/// The largest `propertyupdate` body read. Real clients send a handful of properties.
const MAX_PROPPATCH_BODY: usize = 64 * 1024;

const DAV_NS: &str = "DAV:";
const MS_NS: &str = "urn:schemas-microsoft-com:";

/// One property named in a `propertyupdate` body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Property {
    namespace: Option<String>,
    name: String,
}

impl Property {
    fn is_windows_timestamp(&self) -> bool {
        self.namespace.as_deref() == Some(MS_NS) && self.name.starts_with("Win32")
    }
}

pub(crate) async fn handle_proppatch(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    // A collection's canonical URL ends in `/`; parse it without, as the access
    // middleware does. The raw path stays the `<D:href>` and the `If` comparand.
    let target_path = uri_path
        .strip_suffix('/')
        .filter(|path| !path.is_empty())
        .unwrap_or(&uri_path);
    let Ok(target) = parse_webdav_uri_path(target_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(if_header) = IfHeader::from_headers(req.headers()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    // The target must exist; an object carries the ETag the preconditions compare.
    let etag = match &target {
        DavTarget::Root | DavTarget::KbRoot(_) => None,
        DavTarget::NonDeclaredKb => return StatusCode::FORBIDDEN.into_response(),
        DavTarget::Object(kb, path) => {
            match state
                .storage
                .head_object(kb, path, ConditionalHeaders::default())
                .await
            {
                Ok(meta) => meta.etag,
                Err(error) if error.is_not_found() => {
                    // A virtual folder exists while anything lives beneath it.
                    let prefix = format!("{}/", path.as_str());
                    match state.storage.list_objects(kb, Some(&prefix), 1, None).await {
                        Ok(listing) if !listing.objects.is_empty() => None,
                        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
                        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                }
                Err(StorageError::BackendUnavailable { .. }) => {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };

    let state_for_preconditions = etag.as_deref().map(|etag| ObjectState {
        etag,
        // Write preconditions never read the dates (see `evaluate_write_preconditions`).
        last_modified: std::time::UNIX_EPOCH,
    });
    if evaluate_write_preconditions(state_for_preconditions, &conditionals).is_err()
        || if_header.is_some_and(|header| {
            !header.evaluate(|resource| match resource {
                // Only the request URI is in play; any other resource reads as absent.
                None => etag.as_deref(),
                Some(tag) => tag
                    .parse::<axum::http::Uri>()
                    .ok()
                    .filter(|uri| uri.path() == uri_path)
                    .and(etag.as_deref()),
            })
        })
    {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }

    let Ok(body) = to_bytes(req.into_body(), MAX_PROPPATCH_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Some(properties) = parse_propertyupdate(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let mut response = (
        StatusCode::MULTI_STATUS,
        multistatus(&uri_path, &properties),
    )
        .into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    response
}

/// The properties a `<D:propertyupdate>` sets or removes, in document order, or `None`
/// when the body is not one.
fn parse_propertyupdate(body: &[u8]) -> Option<Vec<Property>> {
    let root = xmltree::Element::parse(body).ok()?;
    if !is_dav(&root, "propertyupdate") {
        return None;
    }
    let mut properties = Vec::new();
    for instruction in elements(&root) {
        if !is_dav(instruction, "set") && !is_dav(instruction, "remove") {
            return None;
        }
        for prop in elements(instruction) {
            if !is_dav(prop, "prop") {
                return None;
            }
            properties.extend(elements(prop).map(|property| Property {
                namespace: property.namespace.clone(),
                name: property.name.clone(),
            }));
        }
    }
    // RFC 4918 §14.19: a propertyupdate holds at least one set or remove.
    (!properties.is_empty()).then_some(properties)
}

fn elements(parent: &xmltree::Element) -> impl Iterator<Item = &xmltree::Element> {
    parent
        .children
        .iter()
        .filter_map(xmltree::XMLNode::as_element)
}

fn is_dav(element: &xmltree::Element, name: &str) -> bool {
    element.namespace.as_deref() == Some(DAV_NS) && element.name == name
}

/// The `207` body: one `propstat` per outcome, listing the properties that share it.
fn multistatus(href: &str, properties: &[Property]) -> String {
    let all_windows = properties.iter().all(Property::is_windows_timestamp);
    let (windows, refused): (Vec<_>, Vec<_>) = properties
        .iter()
        .partition(|property| property.is_windows_timestamp());

    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\"><D:response>",
    );
    let _ = write!(body, "<D:href>{}</D:href>", escape(href));
    if !windows.is_empty() {
        let status = if all_windows {
            "HTTP/1.1 200 OK"
        } else {
            "HTTP/1.1 424 Failed Dependency"
        };
        push_propstat(&mut body, &windows, status, None);
    }
    if !refused.is_empty() {
        push_propstat(
            &mut body,
            &refused,
            "HTTP/1.1 403 Forbidden",
            Some("<D:error><D:cannot-modify-protected-property/></D:error>"),
        );
    }
    body.push_str("</D:response></D:multistatus>");
    body
}

fn push_propstat(body: &mut String, properties: &[&Property], status: &str, error: Option<&str>) {
    body.push_str("<D:propstat><D:prop>");
    for (index, property) in properties.iter().enumerate() {
        match &property.namespace {
            Some(namespace) => {
                let _ = write!(
                    body,
                    "<p{index}:{name} xmlns:p{index}=\"{namespace}\"/>",
                    name = property.name,
                    namespace = escape(namespace),
                );
            }
            None => {
                let _ = write!(body, "<{} xmlns=\"\"/>", property.name);
            }
        }
    }
    let _ = write!(body, "</D:prop><D:status>{status}</D:status>");
    if let Some(error) = error {
        body.push_str(error);
    }
    body.push_str("</D:propstat>");
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::{Property, multistatus, parse_propertyupdate};

    const WINDOWS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:">
  <D:set><D:prop>
    <Z:Win32CreationTime>Mon, 01 Jan 2024 00:00:00 GMT</Z:Win32CreationTime>
    <Z:Win32LastModifiedTime>Mon, 01 Jan 2024 00:00:00 GMT</Z:Win32LastModifiedTime>
  </D:prop></D:set>
</D:propertyupdate>"#;

    #[test]
    fn set_and_remove_properties_are_read_in_order() {
        let body = br#"<D:propertyupdate xmlns:D="DAV:" xmlns:x="urn:x">
            <D:set><D:prop><x:color>red</x:color></D:prop></D:set>
            <D:remove><D:prop><D:displayname/></D:prop></D:remove>
        </D:propertyupdate>"#;
        assert_eq!(
            parse_propertyupdate(body),
            Some(vec![
                Property {
                    namespace: Some("urn:x".into()),
                    name: "color".into()
                },
                Property {
                    namespace: Some("DAV:".into()),
                    name: "displayname".into()
                },
            ])
        );
    }

    #[test]
    fn anything_but_a_propertyupdate_is_refused() {
        for body in [
            &b""[..],
            b"not xml",
            b"<D:propfind xmlns:D=\"DAV:\"><D:allprop/></D:propfind>",
            b"<D:propertyupdate xmlns:D=\"DAV:\"/>",
            b"<D:propertyupdate xmlns:D=\"DAV:\"><D:bogus/></D:propertyupdate>",
            b"<propertyupdate><set><prop><a/></prop></set></propertyupdate>",
        ] {
            assert_eq!(parse_propertyupdate(body), None, "{body:?}");
        }
    }

    #[test]
    fn windows_timestamps_alone_answer_200() {
        let properties = parse_propertyupdate(WINDOWS.as_bytes()).expect("parses");
        let body = multistatus("/webdav/notes/a.md", &properties);
        assert!(body.contains("HTTP/1.1 200 OK"), "{body}");
        assert!(!body.contains("403"), "{body}");
        assert!(body.contains("Win32LastModifiedTime"), "{body}");
    }

    #[test]
    fn a_refused_property_fails_the_windows_ones_too() {
        let body = br#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:">
            <D:set><D:prop><Z:Win32FileAttributes>00000020</Z:Win32FileAttributes>
            <D:getlastmodified>x</D:getlastmodified></D:prop></D:set>
        </D:propertyupdate>"#;
        let properties = parse_propertyupdate(body).expect("parses");
        let body = multistatus("/webdav/notes/a&b.md", &properties);
        assert!(body.contains("HTTP/1.1 424 Failed Dependency"), "{body}");
        assert!(body.contains("HTTP/1.1 403 Forbidden"), "{body}");
        assert!(
            body.contains("<D:cannot-modify-protected-property/>"),
            "{body}"
        );
        assert!(
            body.contains("<D:href>/webdav/notes/a&amp;b.md</D:href>"),
            "{body}"
        );
        assert!(xmltree::Element::parse(body.as_bytes()).is_ok(), "{body}");
    }
}
