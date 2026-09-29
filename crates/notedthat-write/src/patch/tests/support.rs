use super::super::*;
use notedthat_core::StorageError;
use notedthat_core::testing::{ScriptedStorage, StorageOp};
use notedthat_indexer::IndexEvent;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;

const KB: &str = "test-kb";
const PATH: &str = "test.md";

/// A [`ScriptedStorage`] seeded with `test-kb/test.md` at `ETag` `etag1`.
pub(super) struct TestStorage(ScriptedStorage);

/// How many calls to each operation the patch made, failed ones included.
pub(super) struct Calls {
    pub(super) head: usize,
    pub(super) get: usize,
    pub(super) put: usize,
}

#[derive(Default)]
pub(super) struct Script {
    pub(super) get_failures_remaining: u32,
    pub(super) put_failures_remaining: u32,
    /// Move the stored `ETag` to `etag2` as each scripted PUT failure fires, the
    /// way a concurrent writer landing in the window would.
    pub(super) advance_etag_on_put_failure: bool,
}

impl TestStorage {
    pub(super) async fn with_body(body: &'static [u8]) -> Self {
        Self::with_script(body, Script::default()).await
    }

    pub(super) async fn with_script(body: &'static [u8], script: Script) -> Self {
        let storage = ScriptedStorage::default();
        storage
            .inner()
            .seed(KB, PATH, body, Some("text/plain"), Some("etag1"))
            .await;
        let precondition_failed = || StorageError::PreconditionFailed;
        storage.fail_next(
            StorageOp::GetObject,
            script.get_failures_remaining,
            precondition_failed,
        );
        storage.fail_next(
            StorageOp::PutObject,
            script.put_failures_remaining,
            precondition_failed,
        );
        if script.advance_etag_on_put_failure {
            // The hook runs on every PUT, before the scripted failure; counting down
            // in step with the failures keeps it off the PUT that is let through.
            let advances = Arc::new(AtomicU32::new(script.put_failures_remaining));
            storage.before(StorageOp::PutObject, move |store| {
                let advances = Arc::clone(&advances);
                async move {
                    let due = advances
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok();
                    if due {
                        let current = store.object(KB, PATH).await.expect("object exists");
                        store
                            .seed(
                                KB,
                                PATH,
                                current.bytes,
                                current.meta.content_type.as_deref(),
                                Some("etag2"),
                            )
                            .await;
                    }
                }
            });
        }
        Self(storage)
    }

    pub(super) async fn body(&self) -> Bytes {
        self.0
            .inner()
            .object(KB, PATH)
            .await
            .expect("object exists")
            .bytes
    }

    pub(super) fn calls(&self) -> Calls {
        Calls {
            head: self.0.count(StorageOp::HeadObject),
            get: self.0.count(StorageOp::GetObject),
            put: self.0.count(StorageOp::PutObject),
        }
    }
}

pub(super) fn conditionals(etag: Option<&str>) -> ConditionalHeaders {
    ConditionalHeaders {
        if_match: etag.map(str::to_string),
        ..ConditionalHeaders::default()
    }
}

pub(super) async fn run_patch(
    storage: &TestStorage,
    patch_mode: PatchMode,
    caller_conditionals: ConditionalHeaders,
    max_size: u64,
) -> Result<(PutOutcome, mpsc::Receiver<IndexEvent>), WriteError> {
    let (indexer_tx, rx) = mpsc::channel(8);
    let kb = kb();
    let path = path();
    let outcome = patch(
        &storage.0,
        &crate::WriteSinks::indexer_only(&indexer_tx),
        PatchRequest {
            kb: &kb,
            path: &path,
            patch_mode,
            caller_conditionals,
            max_patchable_size: max_size,
            caller_content_type: None,
        },
    )
    .await?;
    Ok((outcome, rx))
}

fn kb() -> KbSlug {
    KbSlug::try_new(KB).expect("valid kb slug")
}

fn path() -> ObjectPath {
    ObjectPath::try_from_str(PATH).expect("valid path")
}
