use notedthat_core::ConditionalHeaders;
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc;

use super::TestStorage;
use crate::replace::{ReplaceRequest, replace};

pub(in crate::replace::tests) struct ReplaceArgs<'a> {
    pub(in crate::replace::tests) old: &'a str,
    pub(in crate::replace::tests) new: &'a str,
    pub(in crate::replace::tests) replace_all: bool,
}

impl<'a> ReplaceArgs<'a> {
    pub(in crate::replace::tests) fn one(old: &'a str, new: &'a str) -> Self {
        Self {
            old,
            new,
            replace_all: false,
        }
    }

    pub(in crate::replace::tests) fn all(old: &'a str, new: &'a str) -> Self {
        Self {
            old,
            new,
            replace_all: true,
        }
    }
}

pub(in crate::replace::tests) async fn run_replace(
    storage: &TestStorage,
    args: ReplaceArgs<'_>,
) -> Result<crate::ReplaceOutcome, crate::WriteError> {
    run_replace_with(storage, args, conditionals(Some("etag1")), 1024, None).await
}

pub(in crate::replace::tests) async fn run_replace_with(
    storage: &TestStorage,
    args: ReplaceArgs<'_>,
    caller_conditionals: ConditionalHeaders,
    max_patchable_size: u64,
    supplied_indexer_tx: Option<mpsc::Sender<IndexEvent>>,
) -> Result<crate::ReplaceOutcome, crate::WriteError> {
    let (indexer_tx, _rx) = mpsc::channel::<IndexEvent>(8);
    let indexer_tx = supplied_indexer_tx.unwrap_or(indexer_tx);
    let kb = kb();
    let path = path();
    replace(
        storage,
        &indexer_tx,
        ReplaceRequest {
            kb: &kb,
            path: &path,
            old_string: args.old,
            new_string: args.new,
            replace_all: args.replace_all,
            caller_conditionals,
            max_patchable_size,
            caller_content_type: None,
        },
    )
    .await
}

pub(in crate::replace::tests) fn expect_replace_err(
    result: Result<crate::ReplaceOutcome, crate::WriteError>,
) -> crate::WriteError {
    match result {
        Ok(_) => panic!("replace should fail"),
        Err(err) => err,
    }
}

pub(in crate::replace::tests) fn conditionals(etag: Option<&str>) -> ConditionalHeaders {
    super::make_conditionals(etag)
}

pub(in crate::replace::tests) fn kb() -> notedthat_core::KbSlug {
    super::make_kb()
}

pub(in crate::replace::tests) fn path() -> notedthat_core::ObjectPath {
    super::make_path()
}
