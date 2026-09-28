//! Where a committed write is announced: the indexing queue and, when one is
//! configured, the object change event log.
//!
//! Bundled so every write path takes one argument and every surface names its
//! [`EventSource`] once, where it builds the bundle, rather than at each call.

use notedthat_core::{EventPublisher, EventSource};
use notedthat_indexer::{IndexHealth, IndexQueueSender};

/// The two places a durable write is reported to, and on whose behalf.
#[derive(Clone)]
pub struct WriteSinks<'a> {
    /// The in-process indexing queue (D38).
    pub indexer_tx: IndexQueueSender,
    /// The event log, when `NOTEDTHAT_EVENTS_BACKEND` selects one.
    pub events: Option<&'a dyn EventPublisher>,
    /// Where what happened at the queue — enqueued, refused, or a queue with
    /// no worker behind it — is recorded for the health view (#97). `None`
    /// only in tests that have no health view to keep.
    pub index_health: Option<&'a IndexHealth>,
    /// The surface making the write, stamped on every event it publishes.
    pub source: EventSource,
}

impl<'a> WriteSinks<'a> {
    /// Both sinks, recording on `index_health`.
    #[must_use]
    pub fn new(
        indexer_tx: &'a IndexQueueSender,
        events: Option<&'a dyn EventPublisher>,
        index_health: &'a IndexHealth,
        source: EventSource,
    ) -> Self {
        Self {
            indexer_tx: indexer_tx.clone(),
            events,
            index_health: Some(index_health),
            source,
        }
    }

    /// The indexing queue alone, as every deployment without an events backend
    /// runs, attributed to the HTTP API, with nothing keeping a health record.
    #[must_use]
    pub fn indexer_only(indexer_tx: impl Into<IndexQueueSender>) -> Self {
        Self {
            indexer_tx: indexer_tx.into(),
            events: None,
            index_health: None,
            source: EventSource::Http,
        }
    }
}
