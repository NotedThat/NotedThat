//! `NotedThat` server — HTTP API in one process.
//! See `docs/CONFIGURATION.md` for env var reference.
#![deny(missing_docs)]

pub mod config;
pub mod provision;
pub mod run;
pub mod tracing_init;

pub use run::WEBDAV_INFLIGHT_GRACE;
