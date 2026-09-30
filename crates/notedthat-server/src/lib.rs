//! `NotedThat` server — HTTP API in one process.
//! See `docs/CONFIGURATION.md` for env var reference.
#![deny(missing_docs)]

pub mod cli;
pub mod config;
pub mod oidc;
pub mod provision;
pub mod run;
/// Test support: in-process servers the workspace's E2E suites drive.
#[cfg(feature = "test-support")]
pub mod testing;
pub mod tracing_init;
