#![allow(missing_docs)]

//! `FsStorage` and `InMemoryStorage`, run through one scenario set and asserted to agree.
//!
//! Needs no container, so it runs in the ordinary `cargo test` pass on every push.
//!
//! `InMemoryStorage` is what every E2E suite in the workspace runs on, and
//! `DEVELOPMENT.md` warns that a green E2E run says nothing about `S3Storage`. This is
//! half of closing that: paired with `storage_conformance_s3.rs`, which compares
//! `FsStorage` against real S3, `FsStorage` is the pivot — if it agrees with both, then
//! by transitivity the substitute agrees with the backend it stands in for, and the
//! cheap half of that check runs without Docker.

#[path = "support/conformance.rs"]
mod conformance;

use conformance::{
    Backend, assert_agree, assert_pinned, assert_premises, observe_all, observe_pinned_divergences,
};
use notedthat_core::{KbSlug, Storage, TenantSlug, testing::InMemoryStorage};
use notedthat_storage_fs::{FsConfig, FsStorage};

#[tokio::test]
async fn fs_storage_and_the_in_memory_substitute_agree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = FsConfig::new(dir.path().to_path_buf());
    let root = notedthat_storage_fs::open_root(&config)
        .await
        .expect("storage root");
    let fs = FsStorage::new(&config, root.root().to_path_buf(), TenantSlug::default());
    let memory = InMemoryStorage::default();

    // One suffix for both, so an observation that echoes the slug still compares.
    let suffix = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() % 1_000_000)
            .unwrap_or_default()
    );

    let from_fs = observe_all(&fs, &suffix, |kb| {
        let fs = fs.clone();
        async move {
            fs.ensure_bucket(&kb).await.expect("bucket");
        }
    })
    .await;
    let from_memory = observe_all(&memory, &suffix, |kb| {
        let memory = memory.clone();
        async move {
            memory.ensure_bucket(&kb).await.expect("bucket");
        }
    })
    .await;

    assert_premises(Backend::Fs, &from_fs);
    assert_premises(Backend::Memory, &from_memory);
    assert_agree(Backend::Fs, &from_fs, Backend::Memory, &from_memory);

    // Behaviours the backends are allowed to differ on are checked against what is
    // recorded, rather than against each other.
    let kb = KbSlug::try_new(format!("conf-div-{suffix}")).expect("slug");
    fs.ensure_bucket(&kb).await.expect("bucket");
    memory.ensure_bucket(&kb).await.expect("bucket");
    assert_pinned(Backend::Fs, &observe_pinned_divergences(&fs, &kb).await);
    assert_pinned(
        Backend::Memory,
        &observe_pinned_divergences(&memory, &kb).await,
    );
}
