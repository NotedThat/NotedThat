//! The knowledge-base manifest is validated where its bytes are stored.
//!
//! `.notedthat/manifest.json` is an ordinary object to every surface, and §3.1
//! puts the rules every surface shares in this crate: HTTP `PUT`, `PATCH` and
//! replace, `WebDAV` `PUT`/`COPY`/`MOVE`, and through them every MCP tool, all
//! end in [`commit`](crate::commit), [`patch`](crate::patch),
//! [`replace`](crate::replace) or [`commit_copy`](crate::commit_copy). A body
//! about to become the manifest is parsed as [`KbManifest`], must name the
//! knowledge base it is written to, and passes [`KbManifest::validate`] — the
//! checks `provision_kbs` makes at startup, with the messages it prints — so a
//! description or access rule outside the limits is refused now with the
//! reason, rather than stored with a success and met as a refused boot by
//! whoever restarts next (#98).

use notedthat_core::{KbManifest, KbSlug, ObjectPath, StagedBody};

use crate::WriteError;

/// The most bytes a manifest may be: a manifest is a few hundred bytes of
/// JSON, and one over this is invalid before it is parsed, so no surface has
/// to buffer an upload of arbitrary size to check it.
pub const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

/// Whether `path` is the knowledge base's manifest.
pub(crate) fn is_manifest(path: &ObjectPath) -> bool {
    path.as_str() == KbManifest::KEY
}

/// Validate `bytes` as the manifest of `kb`, when `path` is the manifest key.
///
/// A no-op for any other key.
pub(crate) fn check_manifest_bytes(
    kb: &KbSlug,
    path: &ObjectPath,
    bytes: &[u8],
) -> Result<(), WriteError> {
    if !is_manifest(path) {
        return Ok(());
    }
    check_size(bytes.len() as u64)?;
    validate_manifest_bytes(kb, bytes)
}

/// Validate a staged body as the manifest of `kb`, when `path` is the manifest
/// key.
///
/// A no-op for any other key. The size is checked before a byte is read, so a
/// spilled body is only ever read back whole when it is small enough to be a
/// manifest at all.
pub(crate) async fn check_staged_manifest(
    kb: &KbSlug,
    path: &ObjectPath,
    body: &StagedBody,
) -> Result<(), WriteError> {
    if !is_manifest(path) {
        return Ok(());
    }
    check_size(body.len())?;
    let bytes = body
        .prefix(usize::try_from(body.len()).unwrap_or(usize::MAX))
        .await
        .map_err(|error| WriteError::InvalidManifest {
            message: format!("manifest could not be read back for validation: {error}"),
        })?;
    validate_manifest_bytes(kb, &bytes)
}

fn check_size(size: u64) -> Result<(), WriteError> {
    if size > MANIFEST_MAX_BYTES {
        return Err(WriteError::InvalidManifest {
            message: format!(
                "manifest is {size} bytes; a manifest is at most {MANIFEST_MAX_BYTES}"
            ),
        });
    }
    Ok(())
}

/// The checks startup makes, in the order it makes them: a manifest document,
/// naming this knowledge base, within the limits.
fn validate_manifest_bytes(kb: &KbSlug, bytes: &[u8]) -> Result<(), WriteError> {
    let manifest: KbManifest =
        serde_json::from_slice(bytes).map_err(|e| WriteError::InvalidManifest {
            message: format!("manifest is not a valid manifest document: {e}"),
        })?;
    if manifest.kb_slug != *kb {
        return Err(WriteError::InvalidManifest {
            message: format!(
                "manifest names knowledge base '{}' but was written to '{}'",
                manifest.kb_slug.as_str(),
                kb.as_str()
            ),
        });
    }
    manifest
        .validate()
        .map_err(|e| WriteError::InvalidManifest {
            message: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    fn manifest_path() -> ObjectPath {
        ObjectPath::try_from_str(KbManifest::KEY).unwrap()
    }

    fn manifest(description: &str) -> String {
        format!(
            r#"{{"notedthat_version":"0.7.2","manifest_version":1,"tenant_slug":"default","kb_slug":"notes","display_name":"Notes","description":"{description}","created_at":1700000000,"access":[]}}"#
        )
    }

    #[test]
    fn an_ordinary_key_is_not_checked() {
        let path = ObjectPath::try_from_str("notes/a.md").unwrap();
        assert!(check_manifest_bytes(&kb(), &path, b"# not json").is_ok());
    }

    #[test]
    fn the_manifest_startup_would_accept_is_accepted() {
        let body = manifest("Engineering notes and ADRs.");
        assert!(check_manifest_bytes(&kb(), &manifest_path(), body.as_bytes()).is_ok());
    }

    #[test]
    fn a_description_outside_the_limits_is_refused_with_the_boot_message() {
        let body = manifest("two\\nlines");
        let error = check_manifest_bytes(&kb(), &manifest_path(), body.as_bytes()).unwrap_err();
        assert!(
            matches!(&error, WriteError::InvalidManifest { message } if message.contains("description")),
            "{error}"
        );
    }

    #[test]
    fn a_body_that_is_not_a_manifest_is_refused() {
        let error = check_manifest_bytes(&kb(), &manifest_path(), b"# not json").unwrap_err();
        assert!(
            matches!(error, WriteError::InvalidManifest { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_manifest_naming_another_knowledge_base_is_refused() {
        let body = manifest("fine").replace(r#""kb_slug":"notes""#, r#""kb_slug":"other""#);
        let error = check_manifest_bytes(&kb(), &manifest_path(), body.as_bytes()).unwrap_err();
        assert!(
            matches!(&error, WriteError::InvalidManifest { message } if message.contains("'other'")),
            "{error}"
        );
    }

    #[test]
    fn a_manifest_over_the_size_bound_is_refused_unparsed() {
        let body = vec![b' '; usize::try_from(MANIFEST_MAX_BYTES).unwrap() + 1];
        let error = check_manifest_bytes(&kb(), &manifest_path(), &body).unwrap_err();
        assert!(
            matches!(&error, WriteError::InvalidManifest { message } if message.contains("at most")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_staged_manifest_is_read_back_and_checked() {
        let good = StagedBody::from(bytes::Bytes::from(manifest("fine")));
        assert!(
            check_staged_manifest(&kb(), &manifest_path(), &good)
                .await
                .is_ok()
        );
        let bad = StagedBody::from(bytes::Bytes::from_static(b"{}"));
        assert!(
            check_staged_manifest(&kb(), &manifest_path(), &bad)
                .await
                .is_err()
        );
    }
}
