use notedthat_core::{KbSlug, ObjectPath, PutOutcome};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

use crate::WriteError;

pub(crate) fn enqueue_patch_upsert(
    indexer_tx: &Sender<IndexEvent>,
    kb: &KbSlug,
    path: &ObjectPath,
    outcome: &PutOutcome,
) -> Result<(), WriteError> {
    let event = IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: path.clone(),
        etag: outcome.etag.clone().unwrap_or_default(),
        mtime: current_unix_seconds(),
    };
    match indexer_tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(ev)) => {
            tracing::warn!(target: "notedthat::indexing", kb = %kb, path = %path, "INDEX_QUEUE_FULL");
            let _ = ev;
            Err(WriteError::IndexerBackpressureUpsert)
        }
        Err(TrySendError::Closed(ev)) => {
            tracing::error!(target: "notedthat::indexing", kb = %kb, path = %path, "INDEX_QUEUE_CLOSED");
            let _ = ev;
            Ok(())
        }
    }
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
