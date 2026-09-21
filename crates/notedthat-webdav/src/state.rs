//! Shared state for the `WebDAV` surface.

use notedthat_core::{
    AccessPolicy, Authenticator, EventPublisher, EventSource, KbSlug, StagingConfig, Storage,
};
use notedthat_indexer::IndexEvent;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

/// Application state shared by `WebDAV` router, middleware, and filesystem handlers.
#[derive(Clone)]
pub struct WebDavState {
    /// The credential rules every request's principal is resolved through.
    ///
    /// The same authenticator the HTTP API holds; this surface additionally
    /// accepts its `Basic` pair, because `WebDAV` clients prompt for one.
    pub authenticator: Arc<Authenticator>,
    /// Object storage backend shared across handlers.
    pub storage: Arc<dyn Storage>,
    /// Declared knowledge bases keyed by display name/path segment.
    pub declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    /// Startup snapshot of manifest access policies, keyed by KB slug.
    ///
    /// The same map the HTTP API holds — one evaluator, both surfaces.
    pub access_policies: Arc<BTreeMap<String, Arc<AccessPolicy>>>,
    /// Indexer event channel used after write operations.
    pub indexer_tx: Sender<IndexEvent>,
    /// Shared private directory used to spool upload bodies.
    pub staging_config: StagingConfig,
    /// The object change event log, when `NOTEDTHAT_EVENTS_BACKEND` selects one.
    pub events: Option<Arc<dyn EventPublisher>>,
    /// The per-knowledge-base index health record, shared with the indexer
    /// worker and every write path (#97).
    pub index_health: Arc<notedthat_indexer::IndexHealth>,
}

impl WebDavState {
    /// Where a write made through this surface is reported.
    pub(crate) fn sinks(&self) -> notedthat_write::WriteSinks<'_> {
        notedthat_write::WriteSinks::new(
            &self.indexer_tx,
            self.events.as_deref(),
            &self.index_health,
            EventSource::Webdav,
        )
    }
}
