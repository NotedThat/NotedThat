//! Per-segment path normalization (D40) for `WebDAV` write and read paths.

use notedthat_core::{KbSlug, ObjectPath};
use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::filesystem::DavTarget;

pub(crate) const WEBDAV_PREFIX: &str = "/webdav";

pub(super) fn strip_webdav_prefix(uri_path: &str) -> Result<&str, ()> {
    match uri_path.strip_prefix(WEBDAV_PREFIX) {
        Some("") => Ok("/"),
        Some(path) if path.starts_with('/') => Ok(path),
        Some(_) | None => Err(()),
    }
}

pub(super) fn parse_webdav_uri_path(
    uri_path: &str,
    declared_kbs: &BTreeMap<String, KbSlug>,
) -> Result<DavTarget, ()> {
    parse_uri_path(strip_webdav_prefix(uri_path)?, declared_kbs)
}

pub(super) fn decode_uri_segment(raw_segment: &str) -> Result<Cow<'_, str>, ()> {
    percent_encoding::percent_decode_str(raw_segment)
        .decode_utf8()
        .map_err(|_| ())
}

pub(super) fn validate_decoded_segment(decoded: &str) -> bool {
    !decoded.is_empty()
        && !decoded.contains('/')
        && decoded != "."
        && decoded != ".."
        && !decoded.contains('\\')
        && !decoded.contains('\0')
}

pub(super) fn parse_uri_path(
    uri_path: &str,
    declared_kbs: &BTreeMap<String, KbSlug>,
) -> Result<DavTarget, ()> {
    let raw_path = uri_path.strip_prefix('/').unwrap_or(uri_path);
    if raw_path.is_empty() {
        return Ok(DavTarget::Root);
    }

    let (kb_raw, rest_raw) = raw_path.split_once('/').unwrap_or((raw_path, ""));
    let kb_decoded = decode_uri_segment(kb_raw)?;
    if !validate_decoded_segment(&kb_decoded) {
        return Err(());
    }

    let Ok(kb_slug) = KbSlug::try_new(kb_decoded.as_ref()) else {
        return Err(());
    };

    if !declared_kbs.contains_key(kb_decoded.as_ref()) {
        return Ok(DavTarget::NonDeclaredKb);
    }

    if rest_raw.is_empty() {
        return Ok(DavTarget::KbRoot(kb_slug));
    }

    let decoded_parts = rest_raw
        .split('/')
        .map(|raw_segment| {
            let decoded = decode_uri_segment(raw_segment)?;
            if !validate_decoded_segment(&decoded) {
                return Err(());
            }
            Ok(decoded)
        })
        .collect::<Result<Vec<_>, ()>>()?;
    let decoded_rest = decoded_parts
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("/");

    ObjectPath::try_from_str(&decoded_rest)
        .map(|path| DavTarget::Object(kb_slug, path))
        .map_err(|_| ())
}

/// Read-side path validation: enforces D40 strict per-segment rules on EVERY
/// segment (including segments after a non-declared first KB slug — otherwise
/// `parse_uri_path`'s early `Ok(NonDeclaredKb)` return for unknown slugs
/// would silently skip validation for the rest of the path, letting
/// `/unknown/%2e%2e/notes/hello.md` reach `dav-server`), and tolerates exactly
/// ONE trailing `/` on legitimate collection paths (e.g. `/notes/folder/`).
/// Still rejects `.`, `..`, empty middle segments, encoded `/`, `\`, `\0`,
/// invalid UTF-8, and any `//` double-slash (raw or from an empty first segment
/// after `//notes/...`).
pub(super) fn validate_read_uri_path(
    uri_path: &str,
    declared_kbs: &BTreeMap<String, KbSlug>,
) -> Result<(), ()> {
    let raw_path = uri_path.strip_prefix('/').unwrap_or(uri_path);
    if raw_path.is_empty() {
        return Ok(());
    }

    // Strip AT MOST one trailing `/` (for legitimate collection paths); a second
    // trailing `/` or an empty stripped path is a double-slash violation.
    let candidate_raw = if let Some(stripped) = raw_path.strip_suffix('/') {
        if stripped.is_empty() || stripped.ends_with('/') {
            return Err(());
        }
        stripped
    } else {
        raw_path
    };

    // Reject any segment that fails D40, INDEPENDENT of KB-declared status.
    for raw_segment in candidate_raw.split('/') {
        let decoded = decode_uri_segment(raw_segment)?;
        if !validate_decoded_segment(&decoded) {
            return Err(());
        }
    }

    // Only after every segment passes D40, resolve the target via `parse_uri_path`.
    let candidate_uri = if raw_path.ends_with('/') {
        uri_path.strip_suffix('/').ok_or(())?
    } else {
        uri_path
    };
    parse_uri_path(candidate_uri, declared_kbs).map(|_| ())
}

pub(super) fn validate_webdav_read_uri_path(
    uri_path: &str,
    declared_kbs: &BTreeMap<String, KbSlug>,
) -> Result<(), ()> {
    validate_read_uri_path(strip_webdav_prefix(uri_path)?, declared_kbs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared_kbs() -> BTreeMap<String, KbSlug> {
        let slug = KbSlug::try_new("notes").expect("`notes` is a valid slug");
        BTreeMap::from([("notes".to_string(), slug)])
    }

    #[test]
    fn strip_webdav_prefix_accepts_only_the_mount_point() {
        assert_eq!(strip_webdav_prefix("/webdav/notes/a.md"), Ok("/notes/a.md"));
        assert_eq!(strip_webdav_prefix("/webdav"), Ok("/"));
        assert_eq!(strip_webdav_prefix("/webdav/"), Ok("/"));

        // A prefix match is not a mount-point match: `/webdavfoo` must not be
        // mistaken for a path inside `/webdav`.
        assert_eq!(strip_webdav_prefix("/webdavfoo/notes/a.md"), Err(()));
        assert_eq!(strip_webdav_prefix("/webdav-backup"), Err(()));
        // The mount point is case-sensitive, and `//webdav` is a different path.
        assert_eq!(strip_webdav_prefix("/WebDAV/notes/a.md"), Err(()));
        assert_eq!(strip_webdav_prefix("//webdav/notes/a.md"), Err(()));
        // Other surfaces on the shared listener, and unprefixed paths.
        assert_eq!(strip_webdav_prefix("/api/v1/knowledgebases"), Err(()));
        assert_eq!(strip_webdav_prefix("/mcp"), Err(()));
        assert_eq!(strip_webdav_prefix("/notes/a.md"), Err(()));
        assert_eq!(strip_webdav_prefix(""), Err(()));
    }

    /// `COPY`/`MOVE` pass the attacker-controlled `Destination` header straight
    /// into [`parse_webdav_uri_path`] without routing, so this function — not
    /// the router — is what confines a destination to `/webdav`.
    #[test]
    fn parse_webdav_uri_path_confines_copy_move_destinations_to_the_mount_point() {
        let kbs = declared_kbs();

        // Inside the mount point and inside a declared KB.
        assert!(matches!(
            parse_webdav_uri_path("/webdav/notes/a.md", &kbs),
            Ok(DavTarget::Object(_, _))
        ));
        // The mount point itself resolves to the collection root, which the
        // COPY/MOVE handlers reject as a non-object destination.
        assert_eq!(parse_webdav_uri_path("/webdav", &kbs), Ok(DavTarget::Root));
        assert_eq!(parse_webdav_uri_path("/webdav/", &kbs), Ok(DavTarget::Root));

        // Outside the mount point.
        for destination in [
            "/webdavfoo/notes/a.md",
            "/WebDAV/notes/a.md",
            "//webdav/notes/a.md",
            "/api/v1/knowledgebases/notes/a.md",
            "/mcp",
            "/notes/a.md",
        ] {
            assert_eq!(
                parse_webdav_uri_path(destination, &kbs),
                Err(()),
                "destination {destination} must not resolve inside /webdav"
            );
        }

        // Inside the mount point but rejected by D40 segment rules: traversal
        // raw and percent-encoded, and empty segments.
        for destination in [
            "/webdav/../api/v1/knowledgebases/notes/a.md",
            "/webdav/%2e%2e/notes/a.md",
            "/webdav/notes/../../etc/passwd",
            "/webdav//notes/a.md",
        ] {
            assert_eq!(
                parse_webdav_uri_path(destination, &kbs),
                Err(()),
                "destination {destination} must be rejected after prefix stripping"
            );
        }
    }
}
