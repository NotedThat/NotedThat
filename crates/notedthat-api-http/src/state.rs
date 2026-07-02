//! Shared application state for the axum router.

use notedthat_core::{KbSlug, Storage};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Application state shared across all axum handlers.
///
/// This is cloned cheaply for each request (all fields are behind [`Arc`]).
#[derive(Clone)]
pub struct AppState {
    /// The backing storage implementation (injected at startup).
    pub storage: Arc<dyn Storage>,
    /// Canonical map of slug string → [`KbSlug`] for declared knowledge bases.
    pub declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    /// Static Bearer token for authenticating API requests.
    pub bearer_token: Arc<String>,
    /// Maximum accepted PUT body size in bytes (16 MiB in M2).
    pub max_body_size: u64,
}
