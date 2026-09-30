//! Bounded ingress for indexing events.
//!
//! A permit is acquired before an event is accepted and travels with it until
//! the worker has finished the complete handler.  Consequently the configured
//! bound covers the channel, per-key lanes, and active pipelines alike.

use crate::IndexEvent;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// The process-wide pending-event limit.
pub const INDEX_QUEUE_CAPACITY: usize = 1024;

/// Create an indexing ingress pair with one permit per accepted event.
#[must_use]
pub fn index_queue(capacity: usize) -> (IndexQueueSender, IndexQueueReceiver) {
    let permits = Arc::new(Semaphore::new(capacity));
    let (tx, rx) = mpsc::unbounded_channel();
    (
        IndexQueueSender {
            inner: SenderInner::Bounded {
                tx,
                permits: permits.clone(),
            },
            capacity,
        },
        IndexQueueReceiver { rx },
    )
}

/// Cloneable producer handle for the indexing ingress.
#[derive(Clone)]
pub struct IndexQueueSender {
    inner: SenderInner,
    capacity: usize,
}

#[derive(Clone)]
enum SenderInner {
    Bounded {
        tx: mpsc::UnboundedSender<QueuedEvent>,
        permits: Arc<Semaphore>,
    },
    Plain(mpsc::Sender<IndexEvent>),
}

impl IndexQueueSender {
    /// Adapt a raw channel for test fixtures that read what was enqueued.
    ///
    /// Nothing drains such a channel into a worker: [`IndexerWorker`] takes only
    /// the [`IndexQueueReceiver`] from [`index_queue`].
    ///
    /// [`IndexerWorker`]: crate::IndexerWorker
    #[must_use]
    pub fn from_mpsc(tx: mpsc::Sender<IndexEvent>) -> Self {
        let capacity = tx.max_capacity();
        Self {
            inner: SenderInner::Plain(tx),
            capacity,
        }
    }
    /// Try to accept an event without waiting for room.
    pub fn try_send(&self, event: IndexEvent) -> Result<(), mpsc::error::TrySendError<IndexEvent>> {
        match &self.inner {
            SenderInner::Plain(tx) => tx.try_send(event),
            SenderInner::Bounded { tx, permits } => {
                let permit = match permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(tokio::sync::TryAcquireError::NoPermits) => {
                        return Err(mpsc::error::TrySendError::Full(event));
                    }
                    Err(tokio::sync::TryAcquireError::Closed) => {
                        return Err(mpsc::error::TrySendError::Closed(event));
                    }
                };
                tx.send(QueuedEvent {
                    event,
                    _permit: permit,
                })
                .map_err(|error| mpsc::error::TrySendError::Closed(error.0.event))
            }
        }
    }

    /// Wait until the pending-event bound has room, then accept an event.
    pub async fn send(&self, event: IndexEvent) -> Result<(), mpsc::error::SendError<IndexEvent>> {
        match &self.inner {
            SenderInner::Plain(tx) => tx.send(event).await,
            SenderInner::Bounded { tx, permits } => {
                let Ok(permit) = permits.clone().acquire_owned().await else {
                    return Err(mpsc::error::SendError(event));
                };
                tx.send(QueuedEvent {
                    event,
                    _permit: permit,
                })
                .map_err(|error| mpsc::error::SendError(error.0.event))
            }
        }
    }

    /// Events accepted but not yet fully handled.
    #[must_use]
    pub fn depth(&self) -> usize {
        match &self.inner {
            SenderInner::Bounded { permits, .. } => self.capacity - permits.available_permits(),
            SenderInner::Plain(tx) => self.capacity - tx.capacity(),
        }
    }

    /// The fixed pending-event limit.
    #[must_use]
    pub const fn max_capacity(&self) -> usize {
        self.capacity
    }

    /// Remaining room before a producer is refused or waits.
    #[must_use]
    pub fn capacity(&self) -> usize {
        match &self.inner {
            SenderInner::Bounded { permits, .. } => permits.available_permits(),
            SenderInner::Plain(tx) => tx.capacity(),
        }
    }
}

impl From<&mpsc::Sender<IndexEvent>> for IndexQueueSender {
    fn from(sender: &mpsc::Sender<IndexEvent>) -> Self {
        Self::from_mpsc(sender.clone())
    }
}

/// Receiver owned by the index scheduler.
///
/// It is only ever built by [`index_queue`], so every event it yields carries
/// the permit that bounds the worker's lanes (#257).
pub struct IndexQueueReceiver {
    rx: mpsc::UnboundedReceiver<QueuedEvent>,
}

impl IndexQueueReceiver {
    /// Receive the next event, retaining its ingress permit until dropped.
    pub async fn recv(&mut self) -> Option<QueuedEvent> {
        self.rx.recv().await
    }

    /// Receive an already available event without waiting.
    pub fn try_recv(&mut self) -> Result<QueuedEvent, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

/// An accepted event. Dropping it releases its pending-event permit.
pub struct QueuedEvent {
    /// The accepted operation.
    pub event: IndexEvent,
    _permit: OwnedSemaphorePermit,
}
