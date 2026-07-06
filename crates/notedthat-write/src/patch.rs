//! Shared PATCH splice primitive for object writes.

use bytes::{Bytes, BytesMut};
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbSlug, LineIndex, LineRange, ObjectPath, PutOutcome, Storage,
    StorageError,
};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::Sender;

use crate::WriteError;

mod indexing;

/// Specifies how an object's bytes should be spliced.
#[derive(Debug, Clone)]
pub enum PatchMode {
    /// Replace a byte range.
    Bytes {
        /// Byte range to replace.
        range: ByteRange,
        /// Replacement bytes.
        body: Bytes,
    },
    /// Replace a line range.
    Lines {
        /// Line range to replace or insertion point.
        range: LineRange,
        /// Replacement bytes.
        body: Bytes,
    },
    /// Append to the end.
    Append {
        /// Bytes to append.
        body: Bytes,
    },
}

/// Request data for one optimistic PATCH operation.
pub struct PatchRequest<'a> {
    /// Knowledge base containing the object.
    pub kb: &'a KbSlug,
    /// Object path to patch.
    pub path: &'a ObjectPath,
    /// Patch splice mode.
    pub patch_mode: PatchMode,
    /// Caller-supplied conditional headers.
    pub caller_conditionals: ConditionalHeaders,
    /// Maximum patchable object size in bytes.
    pub max_patchable_size: u64,
    /// Caller-supplied content type, if any.
    pub caller_content_type: Option<&'a str>,
}

/// Apply one optimistic PATCH attempt using the current HEAD `ETag` as the internal CAS anchor.
///
/// # Errors
/// Returns [`WriteError`] when caller preconditions fail, the requested splice is invalid, the
/// object is too large to patch, storage rejects the internal CAS, or the indexer queue is full.
pub async fn patch(
    storage: &dyn Storage,
    indexer_tx: &Sender<IndexEvent>,
    request: PatchRequest<'_>,
) -> Result<PutOutcome, WriteError> {
    const MAX_ATTEMPTS: u32 = 3;
    let PatchRequest {
        kb,
        path,
        patch_mode,
        caller_conditionals,
        max_patchable_size,
        caller_content_type,
    } = request;

    validate_caller_if_match(&patch_mode, &caller_conditionals)?;

    let mut attempt = 0u32;
    loop {
        attempt += 1;

        let meta = storage
            .head_object(kb, path, ConditionalHeaders::default())
            .await?;
        check_caller_precondition(&patch_mode, &caller_conditionals, meta.etag.as_deref())?;

        if meta.size > max_patchable_size {
            return Err(WriteError::PatchTooLarge {
                size: meta.size,
                limit: max_patchable_size,
            });
        }

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
                tracing::debug!(target: "notedthat::patch", kb = %kb, path = %path, attempt, stage = "get", "PATCH_RETRY_PRECONDITION");
                continue;
            }
            Err(error) => return Err(WriteError::Storage(error)),
        };
        let read_len = bytes_len_u64(read.bytes.len())?;
        let (byte_range, new_len) = splice_plan(&patch_mode, &read.bytes, read_len)?;

        if new_len > max_patchable_size {
            return Err(WriteError::PatchTooLarge {
                size: new_len,
                limit: max_patchable_size,
            });
        }

        let new_bytes = match (&patch_mode, byte_range) {
            (PatchMode::Bytes { body, .. } | PatchMode::Lines { body, .. }, Some(br)) => {
                let start =
                    usize::try_from(br.start).map_err(|_| WriteError::PatchInvalidRange {
                        message: "splice start does not fit usize".into(),
                    })?;
                let end = usize::try_from(br.end).map_err(|_| WriteError::PatchInvalidRange {
                    message: "splice end does not fit usize".into(),
                })?;
                splice_bytes(&read.bytes, start..end, body)
            }
            (PatchMode::Append { body }, None) => {
                let mut buf = BytesMut::with_capacity(capacity_from_u64(new_len)?);
                buf.extend_from_slice(&read.bytes);
                buf.extend_from_slice(body);
                buf.freeze()
            }
            (PatchMode::Bytes { .. } | PatchMode::Lines { .. }, None)
            | (PatchMode::Append { .. }, Some(_)) => {
                return Err(WriteError::PatchInvalidRange {
                    message: "internal patch mode/range contradiction".into(),
                });
            }
        };

        let put_conditionals = ConditionalHeaders {
            if_match: Some(head_etag),
            ..ConditionalHeaders::default()
        };
        let content_type = caller_content_type
            .or(read.meta.content_type.as_deref())
            .unwrap_or("application/octet-stream");
        let outcome = match storage
            .put_object(kb, path, new_bytes, Some(content_type), put_conditionals)
            .await
        {
            Ok(outcome) => outcome,
            Err(StorageError::PreconditionFailed) if attempt < MAX_ATTEMPTS => {
                tracing::debug!(target: "notedthat::patch", kb = %kb, path = %path, attempt, stage = "put", "PATCH_RETRY_PRECONDITION");
                continue;
            }
            Err(error) => return Err(WriteError::Storage(error)),
        };

        indexing::enqueue_patch_upsert(indexer_tx, kb, path, &outcome)?;

        return Ok(outcome);
    }
}

pub(crate) fn splice_bytes(src: &Bytes, byte_range: std::ops::Range<usize>, body: &Bytes) -> Bytes {
    let mut buf =
        BytesMut::with_capacity(byte_range.start + body.len() + (src.len() - byte_range.end));
    buf.extend_from_slice(&src[..byte_range.start]);
    buf.extend_from_slice(body);
    buf.extend_from_slice(&src[byte_range.end..]);
    buf.freeze()
}

fn validate_caller_if_match(
    patch_mode: &PatchMode,
    caller_conditionals: &ConditionalHeaders,
) -> Result<(), WriteError> {
    match patch_mode {
        PatchMode::Bytes { .. } | PatchMode::Lines { .. } => {
            if caller_conditionals.if_match.is_none() {
                return Err(WriteError::PatchInvalidRange {
                    message: "If-Match required on PATCH (bytes/lines mode)".into(),
                });
            }
        }
        PatchMode::Append { .. } => {}
    }
    if let Some(etag) = &caller_conditionals.if_match
        && (etag == "*" || etag.contains(','))
    {
        return Err(WriteError::PatchInvalidRange {
            message: "If-Match: * and multi-value If-Match not supported on PATCH in v1".into(),
        });
    }
    Ok(())
}

fn check_caller_precondition(
    patch_mode: &PatchMode,
    caller_conditionals: &ConditionalHeaders,
    current_etag: Option<&str>,
) -> Result<(), WriteError> {
    let should_check_caller =
        !matches!(patch_mode, PatchMode::Append { .. }) || caller_conditionals.if_match.is_some();
    if should_check_caller
        && let Some(caller_etag) = &caller_conditionals.if_match
        && current_etag != Some(caller_etag.as_str())
    {
        return Err(WriteError::Storage(StorageError::PreconditionFailed));
    }
    Ok(())
}

fn splice_plan(
    patch_mode: &PatchMode,
    bytes: &Bytes,
    read_len: u64,
) -> Result<(Option<std::ops::Range<u64>>, u64), WriteError> {
    match patch_mode {
        PatchMode::Bytes { range, body } => {
            let br = range.to_exclusive_range(read_len).ok_or_else(|| {
                WriteError::PatchInvalidRange {
                    message: format!("byte range unsatisfiable at size {}", bytes.len()),
                }
            })?;
            let new_len = spliced_len(read_len, br.end - br.start, body.len(), "byte-range")?;
            Ok((Some(br), new_len))
        }
        PatchMode::Lines { range, body } => {
            let idx = LineIndex::from_bytes(bytes);
            let br = idx.byte_range(range).ok_or_else(|| {
                let (first, last) = line_range_bounds(range);
                WriteError::PatchLineOutOfRange {
                    first,
                    last,
                    total_lines: idx.total_lines,
                    total_bytes: idx.total_bytes,
                }
            })?;
            let new_len = spliced_len(read_len, br.end - br.start, body.len(), "line-range")?;
            Ok((Some(br), new_len))
        }
        PatchMode::Append { body } => {
            let body_len = bytes_len_u64(body.len())?;
            let new_len =
                read_len
                    .checked_add(body_len)
                    .ok_or_else(|| WriteError::PatchInvalidRange {
                        message: "append length overflows u64".into(),
                    })?;
            Ok((None, new_len))
        }
    }
}

fn spliced_len(
    read_len: u64,
    replaced: u64,
    body_len: usize,
    mode: &str,
) -> Result<u64, WriteError> {
    read_len
        .checked_sub(replaced)
        .and_then(|n| n.checked_add(bytes_len_u64(body_len).ok()?))
        .ok_or_else(|| WriteError::PatchInvalidRange {
            message: format!("{mode} splice length overflows u64"),
        })
}

fn bytes_len_u64(len: usize) -> Result<u64, WriteError> {
    u64::try_from(len).map_err(|_| WriteError::PatchInvalidRange {
        message: "buffer length does not fit u64".into(),
    })
}

fn capacity_from_u64(len: u64) -> Result<usize, WriteError> {
    usize::try_from(len).map_err(|_| WriteError::PatchInvalidRange {
        message: "patched length does not fit usize".into(),
    })
}

fn line_range_bounds(range: &LineRange) -> (u64, u64) {
    match range {
        LineRange::FromStart { first, last } => (*first, *last),
        LineRange::FromStartOpen { first } => (*first, u64::MAX),
        LineRange::Suffix { length } => (0, *length),
        LineRange::Insert { before } => (*before, before.saturating_sub(1)),
    }
}

#[cfg(test)]
mod tests {
    mod retry;
    pub mod skeleton;
    mod support;
}
