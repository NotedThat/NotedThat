use notedthat_core::testing::{ScriptedStorage, StorageOp};
use notedthat_core::{KbSlug, StorageError};

/// The knowledge bases the fixture declares, provisioned so reads reach the store.
const KBS: [&str; 2] = ["discoverable", "private"];

/// A store holding `objects` as `(kb, key)` pairs, each the 7 bytes `content`.
///
/// These tests only read: provisioning, the manifest and every mutation fail, so a
/// request that reaches one of them shows up as a backend error, not a quiet success.
pub(super) async fn memory_storage(
    objects: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> ScriptedStorage {
    let kbs = KBS.map(|kb| KbSlug::try_new(kb).expect("valid KB slug"));
    let storage = ScriptedStorage::with_kbs(&kbs);
    for (kb, key) in objects {
        storage
            .inner()
            .seed(
                kb,
                key,
                &b"content"[..],
                Some("text/plain"),
                Some(&format!("\"{key}\"")),
            )
            .await;
    }
    for op in [
        StorageOp::EnsureBucket,
        StorageOp::ReadManifest,
        StorageOp::WriteManifest,
        StorageOp::PutObject,
        StorageOp::PutStagedObject,
        StorageOp::CopyObject,
        StorageOp::DeleteObject,
    ] {
        storage.fail_always(op, || StorageError::BackendUnavailable {
            message: "operation is outside this public-read test".to_string(),
        });
    }
    storage
}
