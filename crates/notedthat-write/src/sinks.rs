//! Where a committed write is announced: the indexing queue and, when one is
//! configured, the object change event log.
//!
//! Bundled so every write path takes one argument and every surface names its
//! [`EventSource`] once, where it builds the bundle, rather than at each call.

use notedthat_core::{EventPublisher, EventSource};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::Sender;

/// The two places a durable write is reported to, and on whose behalf.
#[derive(Clone, Copy)]
pub struct WriteSinks<'a> {
    /// The in-process indexing queue (D38).
    pub indexer_tx: &'a Sender<IndexEvent>,
    /// The event log, when `NOTEDTHAT_EVENTS_BACKEND` selects one.
    pub events: Option<&'a dyn EventPublisher>,
    /// The surface making the write, stamped on every event it publishes.
    pub source: EventSource,
}

impl<'a> WriteSinks<'a> {
    /// Both sinks.
    #[must_use]
    pub fn new(
        indexer_tx: &'a Sender<IndexEvent>,
        events: Option<&'a dyn EventPublisher>,
        source: EventSource,
    ) -> Self {
        Self {
            indexer_tx,
            events,
            source,
        }
    }

    /// The indexing queue alone, as every deployment without an events backend
    /// runs, attributed to the HTTP API.
    #[must_use]
    pub fn indexer_only(indexer_tx: &'a Sender<IndexEvent>) -> Self {
        Self::new(indexer_tx, None, EventSource::Http)
    }
}
