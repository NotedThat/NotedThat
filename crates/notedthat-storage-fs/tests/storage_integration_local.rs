#![allow(missing_docs)]

//! The storage integration suite, run against a real directory tree.
//!
//! Every test body lives in `support/integration_scenarios.rs` and is shared with
//! `storage_integration_s3.rs` and `storage_integration_memory.rs`, which run the same
//! bodies against a real S3 server and against the in-memory substitute. This file
//! contributes only the fixture: a temporary root, an `FsStorage` over it, and one
//! knowledge base per scenario.
//!
//! Nothing here needs Docker, so it runs in the ordinary `cargo test` pass — the same
//! assertions that only reach real S3 in CI's integration job are checked against the
//! filesystem backend on every push.
//!
//! Run with: `cargo test -p notedthat-storage-fs --test storage_integration_local`

#[path = "support/integration_scenarios.rs"]
mod scenarios;

use notedthat_core::{KbSlug, TenantSlug};
use notedthat_storage_fs::{FsConfig, FsStorage, RootLock, open_root};
use scenarios::{kb_for, storage_integration_scenarios};

/// A root of its own per scenario, so a scenario cannot see another's tree — and so the
/// `RootLock`, which is exclusive per root, is never contended between concurrent tests.
struct Fixture {
    _dir: tempfile::TempDir,
    _lock: RootLock,
    storage: FsStorage,
    kb: KbSlug,
}

async fn fixture(scenario: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = FsConfig::new(dir.path().to_path_buf());
    let lock = open_root(&config).await.expect("storage root");
    let storage = FsStorage::new(&config, lock.root().to_path_buf(), TenantSlug::default());
    Fixture {
        _dir: dir,
        _lock: lock,
        storage,
        kb: kb_for(scenario),
    }
}

macro_rules! fs_scenario {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let fixture = fixture(stringify!($name)).await;
            scenarios::$name(&fixture.storage, &fixture.kb).await;
        }
    };
}

storage_integration_scenarios!(fs_scenario);
