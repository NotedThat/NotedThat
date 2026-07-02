//! HTTP API surface for `NotedThat` — axum router with static-Bearer auth.
#![deny(missing_docs)]

pub mod error;
pub mod middleware;
pub mod router;
pub mod state;
pub mod write_path;

// Exposed under `test-support` feature for integration tests in `tests/*.rs`
// and under `cfg(test)` for unit tests within `src/`.
// See module documentation for the rationale behind the feature-flag pattern.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
