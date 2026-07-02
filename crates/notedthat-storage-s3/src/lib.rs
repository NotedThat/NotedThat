//! AWS S3 adapter for `NotedThat` — implements [`notedthat_core::Storage`] against `aws-sdk-s3`.
//!
//! Only use this crate from `notedthat-server`. The `notedthat-api-http` crate
//! uses only the [`notedthat_core::Storage`] trait (no S3 dependency).

pub mod config;
pub mod storage;

pub use config::S3Config;
pub use storage::S3Storage;
