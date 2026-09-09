//! Reading a directory level out of flat storage, under the caller's access rules.
//!
//! The order matters and is not negotiable: enumerate, drop what the caller may
//! not see, *then* roll up. Filtering before the rollup is what makes glob
//! scoping work with no extra machinery — a folder is synthesised only from keys
//! that survived, so a grant on `public/**` produces exactly one folder row at
//! the knowledge-base root and nothing else. There is no separate "is this
//! folder permitted" question to answer, and there could not be: a folder is not
//! a thing, it is a shared prefix of permitted keys.
//!
//! The cost is unavoidable. `Storage::list_objects` is flat — no delimiter, no
//! common-prefix rollup — and rules are per key, so a directory page reads every
//! key under its prefix even when most are denied.

use crate::authz::KbAccess;
use notedthat_core::{
    KbSlug, KeyFilter, ObjectMeta, ObjectPath, Rollup, Storage, StorageError, Verb,
    is_internal_path, roll_up,
};

/// Rows a browse page reads from storage before it stops and says so.
///
/// The same number as `WebDAV`'s `PROPFIND_MAX_ENTRIES`, so operators learn one
/// figure — but a separate constant, because the two surfaces degrade
/// differently and should be free to diverge.
pub(super) const BROWSE_MAX_KEYS: usize = 10_000;

/// Rows fetched per storage call while assembling a page.
const SCAN_PAGE: u32 = 1000;

/// One directory level, plus whether the read was complete.
pub(super) struct DirectoryListing {
    pub(super) rollup: Rollup,
    /// `true` when the cap stopped the read before storage was exhausted.
    pub(super) truncated: bool,
    /// The last key read, named in the truncation notice so a reader knows
    /// where the truth stops.
    pub(super) last_key: Option<String>,
}

/// Read the directory level directly under `prefix`.
///
/// `prefix` is empty (the knowledge-base root) or ends with `/`.
pub(super) async fn read_directory(
    storage: &dyn Storage,
    kb: &KbSlug,
    prefix: &str,
    access: &KbAccess,
) -> Result<DirectoryListing, StorageError> {
    let filter = access.filter(Verb::List);
    let scan_prefix = (!prefix.is_empty()).then(|| prefix.to_string());

    let mut visible: Vec<ObjectMeta> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut scanned = 0_usize;
    let mut truncated = false;
    let mut last_key = None;

    loop {
        let page = storage
            .list_objects(kb, scan_prefix.as_deref(), SCAN_PAGE, cursor.as_deref())
            .await?;

        for object in page.objects {
            scanned += 1;
            if scanned > BROWSE_MAX_KEYS {
                truncated = true;
                break;
            }
            last_key = Some(object.key.clone());
            if is_visible(&object.key, &filter) {
                visible.push(object);
            }
        }

        if truncated {
            break;
        }
        let Some(next) = page.next_cursor else { break };
        if cursor.as_deref() == Some(next.as_str()) {
            return Err(StorageError::BackendUnavailable {
                message: "storage returned a non-advancing cursor".into(),
            });
        }
        cursor = Some(next);
    }

    Ok(DirectoryListing {
        rollup: roll_up(visible, prefix),
        truncated,
        last_key,
    })
}

/// Whether a stored key may appear on a browse page at all.
fn is_visible(key: &str, filter: &KeyFilter<'_>) -> bool {
    // The internal namespace is filtered for *every* principal here, not only
    // anonymous ones. The access model only guarantees that `.notedthat` is
    // ungrantable to `anyone`; browse goes further because that namespace is
    // server bookkeeping rather than documents, and these pages are for reading
    // documents. A credentialed operator still reaches it through the API.
    if is_internal_path(key) {
        return false;
    }
    // A key with a `..` segment is legal in S3 and rejected by our write path,
    // so it should not exist — but if one does, its link would resolve out of
    // the knowledge base in the browser. Dropping it here is what makes the
    // link-safety argument in `super::links` hold.
    if ObjectPath::try_from_str(key).is_err() {
        tracing::debug!(key, "browse: omitting an unrepresentable object key");
        return false;
    }
    filter.allows(key)
}

/// Whether a knowledge base has anything at all under `prefix` for this caller.
///
/// Used to tell "an empty folder" (which cannot exist — folders are synthesised
/// from keys) from "a folder that is not yours".
pub(super) fn is_present(listing: &DirectoryListing) -> bool {
    !listing.rollup.is_empty()
}
