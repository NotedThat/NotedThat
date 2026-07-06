//! Shared exact-substring replacement primitive for object writes.

use bytes::{Bytes, BytesMut};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, StorageError};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::Sender;

use crate::{ReplaceOutcome, WriteError};

/// Request data for one optimistic replace operation.
pub struct ReplaceRequest<'a> {
    /// Knowledge base containing the object.
    pub kb: &'a KbSlug,
    /// Object path to replace within.
    pub path: &'a ObjectPath,
    /// Exact UTF-8 substring to search for in the object body.
    pub old_string: &'a str,
    /// Replacement UTF-8 string to splice into the object body.
    pub new_string: &'a str,
    /// Whether to replace every non-overlapping match instead of exactly one match.
    pub replace_all: bool,
    /// Caller-supplied conditional headers.
    pub caller_conditionals: ConditionalHeaders,
    /// Maximum replaceable object size in bytes.
    pub max_patchable_size: u64,
    /// Caller-supplied content type, if any.
    pub caller_content_type: Option<&'a str>,
}

/// Replace exact occurrences of a UTF-8 substring in an object body.
///
/// # Errors
/// Returns [`crate::WriteError`] when storage access, preconditions, size limits, or
/// match-count requirements fail.
pub async fn replace(
    storage: &dyn Storage,
    indexer_tx: &Sender<IndexEvent>,
    request: ReplaceRequest<'_>,
) -> Result<ReplaceOutcome, WriteError> {
    const MAX_ATTEMPTS: u32 = 3;
    let ReplaceRequest {
        kb,
        path,
        old_string,
        new_string,
        replace_all,
        caller_conditionals,
        max_patchable_size,
        caller_content_type,
    } = request;

    if old_string.is_empty() {
        return Err(WriteError::PatchInvalidRange {
            message: "old_string must be non-empty (would match every byte position)".into(),
        });
    }
    crate::patch::require_strong_if_match(&caller_conditionals)?;

    let mut attempt = 0u32;
    loop {
        attempt += 1;

        let meta = storage
            .head_object(kb, path, ConditionalHeaders::default())
            .await?;
        ensure_caller_etag_matches(&caller_conditionals, meta.etag.as_deref())?;
        ensure_within_patchable_size(meta.size, max_patchable_size)?;

        let head_etag = meta
            .etag
            .clone()
            .ok_or_else(|| WriteError::PatchInvalidRange {
                message: "backend did not return ETag on HEAD".into(),
            })?;

        let get_conditionals = ConditionalHeaders {
            if_match: Some(head_etag.clone()),
            ..ConditionalHeaders::default()
        };
        let read = match storage.get_object(kb, path, None, get_conditionals).await {
            Ok(read) => read,
            Err(StorageError::PreconditionFailed) if attempt < MAX_ATTEMPTS => {
                tracing::debug!(target: "notedthat::replace", kb = %kb, path = %path, attempt, stage = "get", "REPLACE_RETRY_PRECONDITION");
                continue;
            }
            Err(error) => return Err(WriteError::Storage(error)),
        };

        let needle = old_string.as_bytes();
        let haystack: &[u8] = read.bytes.as_ref();
        let matches = find_non_overlapping_matches(haystack, needle);

        if matches.is_empty() {
            return Err(WriteError::ReplaceNoMatch);
        }
        if matches.len() >= 2 && !replace_all {
            let count =
                u64::try_from(matches.len()).map_err(|_| WriteError::PatchInvalidRange {
                    message: "replace: match count exceeds u64".into(),
                })?;
            return Err(WriteError::ReplaceAmbiguous { count });
        }

        let match_bound = if replace_all { matches.len() } else { 1 };
        let new_bytes = splice_replacement(&ReplacementSplice {
            haystack,
            needle,
            replacement: new_string.as_bytes(),
            matches: &matches,
            match_bound,
            max_patchable_size,
        })?;

        let put_conditionals = ConditionalHeaders {
            if_match: Some(head_etag),
            ..ConditionalHeaders::default()
        };
        let content_type = caller_content_type
            .or(read.meta.content_type.as_deref())
            .unwrap_or("application/octet-stream");
        let put_outcome = match storage
            .put_object(kb, path, new_bytes, Some(content_type), put_conditionals)
            .await
        {
            Ok(outcome) => outcome,
            Err(StorageError::PreconditionFailed) if attempt < MAX_ATTEMPTS => {
                tracing::debug!(target: "notedthat::replace", kb = %kb, path = %path, attempt, stage = "put", "REPLACE_RETRY_PRECONDITION");
                continue;
            }
            Err(error) => return Err(WriteError::Storage(error)),
        };

        crate::patch::indexing::enqueue_patch_upsert(indexer_tx, kb, path, &put_outcome)?;

        let match_count =
            u64::try_from(match_bound).map_err(|_| WriteError::PatchInvalidRange {
                message: "replace: match count exceeds u64".into(),
            })?;
        return Ok(ReplaceOutcome {
            put_outcome,
            match_count,
        });
    }
}

fn ensure_caller_etag_matches(
    caller_conditionals: &ConditionalHeaders,
    current_etag: Option<&str>,
) -> Result<(), WriteError> {
    if caller_conditionals.if_match.as_deref() != current_etag {
        return Err(WriteError::Storage(StorageError::PreconditionFailed));
    }
    Ok(())
}

fn ensure_within_patchable_size(size: u64, limit: u64) -> Result<(), WriteError> {
    if size > limit {
        return Err(WriteError::PatchTooLarge { size, limit });
    }
    Ok(())
}

fn find_non_overlapping_matches(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut matches = Vec::new();
    let mut cursor = 0usize;
    while cursor + needle.len() <= haystack.len() {
        if &haystack[cursor..cursor + needle.len()] == needle {
            matches.push(cursor);
            cursor += needle.len();
        } else {
            cursor += 1;
        }
    }
    matches
}

struct ReplacementSplice<'a> {
    haystack: &'a [u8],
    needle: &'a [u8],
    replacement: &'a [u8],
    matches: &'a [usize],
    match_bound: usize,
    max_patchable_size: u64,
}

fn splice_replacement(splice: &ReplacementSplice<'_>) -> Result<Bytes, WriteError> {
    let replaced_bytes = splice.match_bound * splice.needle.len();
    let added_bytes = splice.match_bound * splice.replacement.len();
    let new_len = splice
        .haystack
        .len()
        .checked_sub(replaced_bytes)
        .and_then(|n| n.checked_add(added_bytes))
        .ok_or_else(|| WriteError::PatchInvalidRange {
            message: "replace: length arithmetic overflow".into(),
        })?;
    let new_len_u64 = u64::try_from(new_len).map_err(|_| WriteError::PatchInvalidRange {
        message: "replace: new_len exceeds u64".into(),
    })?;
    if new_len_u64 > splice.max_patchable_size {
        return Err(WriteError::PatchTooLarge {
            size: new_len_u64,
            limit: splice.max_patchable_size,
        });
    }

    let mut result = BytesMut::with_capacity(new_len);
    let mut prev_end = 0usize;
    for &matched_at in splice.matches.iter().take(splice.match_bound) {
        result.extend_from_slice(&splice.haystack[prev_end..matched_at]);
        result.extend_from_slice(splice.replacement);
        prev_end = matched_at + splice.needle.len();
    }
    result.extend_from_slice(&splice.haystack[prev_end..]);
    Ok(result.freeze())
}

#[cfg(test)]
mod tests;
