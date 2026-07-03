//! Shared state for the `WebDAV` surface.

use notedthat_core::{KbSlug, Storage};
use notedthat_indexer::IndexEvent;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

/// Application state shared by `WebDAV` router, middleware, and filesystem handlers.
#[derive(Clone)]
pub struct WebDavState {
    /// Static HTTP Basic authentication username.
    pub username: Arc<String>,
    /// Static HTTP Basic authentication password.
    pub password: Arc<String>,
    /// Object storage backend shared across handlers.
    pub storage: Arc<dyn Storage>,
    /// Declared knowledge bases keyed by display name/path segment.
    pub declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    /// Indexer event channel used after write operations.
    pub indexer_tx: Sender<IndexEvent>,
}
