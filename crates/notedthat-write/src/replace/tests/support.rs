use bytes::Bytes;
use notedthat_core::testing::{ScriptedStorage, StorageOp};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, ObjectRead, StorageError};

mod runner;

pub(in crate::replace::tests) use runner::{
    ReplaceArgs, conditionals, expect_replace_err, kb, path, run_replace, run_replace_with,
};

const KB: &str = "test-kb";
const PATH: &str = "test.md";

/// A [`ScriptedStorage`] seeded with `test-kb/test.md` at `ETag` `etag1`.
pub(super) struct TestStorage(ScriptedStorage);

/// How many calls to each operation the replace made, failed ones included.
pub(super) struct Calls {
    pub(super) head: usize,
    pub(super) get: usize,
    pub(super) put: usize,
}

#[derive(Default)]
pub(super) struct Script {
    pub(super) get_failures_remaining: u32,
    pub(super) put_failures_remaining: u32,
}

impl TestStorage {
    pub(super) async fn with_body(body: &'static [u8]) -> Self {
        Self::with_bytes(Bytes::from_static(body), Some("text/plain")).await
    }

    pub(super) async fn with_body_and_content_type(
        body: &'static [u8],
        content_type: &str,
    ) -> Self {
        Self::with_bytes(Bytes::from_static(body), Some(content_type)).await
    }

    pub(super) async fn with_bytes(body: Bytes, content_type: Option<&str>) -> Self {
        let storage = ScriptedStorage::default();
        storage
            .inner()
            .seed(KB, PATH, body, content_type, Some("etag1"))
            .await;
        Self(storage)
    }

    pub(super) async fn with_script(body: &'static [u8], script: Script) -> Self {
        let storage = Self::with_body(body).await;
        let precondition_failed = || StorageError::PreconditionFailed;
        storage.0.fail_next(
            StorageOp::GetObject,
            script.get_failures_remaining,
            precondition_failed,
        );
        storage.0.fail_next(
            StorageOp::PutObject,
            script.put_failures_remaining,
            precondition_failed,
        );
        storage
    }

    /// The stored object, read past the script so it neither counts as a call nor
    /// trips a scripted failure.
    pub(super) async fn read(&self) -> ObjectRead {
        self.0
            .inner()
            .object(KB, PATH)
            .await
            .expect("object exists")
    }

    pub(super) async fn body(&self) -> Bytes {
        self.read().await.bytes
    }

    pub(super) fn calls(&self) -> Calls {
        Calls {
            head: self.0.count(StorageOp::HeadObject),
            get: self.0.count(StorageOp::GetObject),
            put: self.0.count(StorageOp::PutObject),
        }
    }
}

fn make_conditionals(etag: Option<&str>) -> ConditionalHeaders {
    ConditionalHeaders {
        if_match: etag.map(str::to_string),
        ..ConditionalHeaders::default()
    }
}

fn make_kb() -> KbSlug {
    KbSlug::try_new(KB).expect("valid kb slug")
}

fn make_path() -> ObjectPath {
    ObjectPath::try_from_str(PATH).expect("valid path")
}
