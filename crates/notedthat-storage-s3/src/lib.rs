//! AWS S3 adapter for `NotedThat` — implements [`notedthat_core::Storage`] against `aws-sdk-s3`.
//!
//! Only use this crate from `notedthat-server`. The `notedthat-api-http` crate
//! uses only the [`notedthat_core::Storage`] trait (no S3 dependency).
//!
//! # Where this crate's tests are
//!
//! `cargo test -p notedthat-storage-s3` runs no backend coverage, and that is deliberate
//! rather than an oversight: the storage integration suite is written once against
//! `&dyn Storage` and expanded per backend, so it lives with the expansions in
//! `notedthat-storage-fs/tests/`. [`S3Storage`]'s share of it is
//! `storage_integration_s3.rs`, which runs the shared scenarios over a `SeaweedFS`
//! container, and `storage_conformance_s3.rs`, which checks it against `FsStorage`.
//! Both are `#[ignore]`:
//!
//! ```sh
//! cargo test -p notedthat-storage-fs --locked -- --include-ignored
//! ```
//!
//! So a green `cargo test -p notedthat-storage-s3` says nothing about this adapter. Run
//! that command after changing [`storage`], or let CI's integration job do it.

pub mod config;
pub mod storage;

pub use config::{S3_ENV_VARS, S3Config, S3Settings};
pub use storage::S3Storage;
