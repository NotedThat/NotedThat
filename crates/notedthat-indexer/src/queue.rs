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
            tx,
            raw_tx: None,
            permits: permits.clone(),
            capacity,
        },
        IndexQueueReceiver {
            inner: ReceiverInner::Bounded(rx),
        },
    )
}

/// Cloneable producer handle for the indexing ingress.
#[derive(Clone)]
pub struct IndexQueueSender {
    tx: mpsc::UnboundedSender<QueuedEvent>,
    raw_tx: Option<mpsc::Sender<IndexEvent>>,
    permits: Arc<Semaphore>,
    capacity: usize,
}

impl IndexQueueSender {
    /// Adapt an existing bounded channel for test fixtures and legacy callers.
    #[must_use]
    pub fn from_mpsc(tx: mpsc::Sender<IndexEvent>) -> Self {
        let capacity = tx.max_capacity();
        let (unused, _) = mpsc::unbounded_channel();
        Self {
            tx: unused,
            raw_tx: Some(tx),
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }
    /// Try to accept an event without waiting for room.
    pub fn try_send(&self, event: IndexEvent) -> Result<(), mpsc::error::TrySendError<IndexEvent>> {
        if let Some(tx) = &self.raw_tx {
            return tx.try_send(event);
        }
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Err(mpsc::error::TrySendError::Full(event));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(mpsc::error::TrySendError::Closed(event));
            }
        };
        self.tx
            .send(QueuedEvent {
                event,
                _permit: Some(permit),
            })
            .map_err(|error| {
                let queued = error.0;
                mpsc::error::TrySendError::Closed(queued.event)
            })
    }

    /// Wait until the pending-event bound has room, then accept an event.
    pub async fn send(&self, event: IndexEvent) -> Result<(), mpsc::error::SendError<IndexEvent>> {
        if let Some(tx) = &self.raw_tx {
            return tx.send(event).await;
        }
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            return Err(mpsc::error::SendError(event));
        };
        self.tx
            .send(QueuedEvent {
                event,
                _permit: Some(permit),
            })
            .map_err(|error| mpsc::error::SendError(error.0.event))
    }

    /// Events accepted but not yet fully handled.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.capacity - self.permits.available_permits()
    }

    /// The fixed pending-event limit.
    #[must_use]
    pub const fn max_capacity(&self) -> usize {
        self.capacity
    }

    /// Remaining room before a producer is refused or waits.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.raw_tx
            .as_ref()
            .map_or_else(|| self.permits.available_permits(), mpsc::Sender::capacity)
    }
}

impl From<&IndexQueueSender> for IndexQueueSender {
    fn from(sender: &IndexQueueSender) -> Self {
        sender.clone()
    }
}

impl From<&mpsc::Sender<IndexEvent>> for IndexQueueSender {
    fn from(sender: &mpsc::Sender<IndexEvent>) -> Self {
        Self::from_mpsc(sender.clone())
    }
}

/// Receiver owned by the index scheduler.
pub struct IndexQueueReceiver {
    inner: ReceiverInner,
}

enum ReceiverInner {
    Bounded(mpsc::UnboundedReceiver<QueuedEvent>),
    Plain(mpsc::Receiver<IndexEvent>),
}

impl IndexQueueReceiver {
    /// Adapt a raw channel for compatibility with embedders and test fixtures.
    #[must_use]
    pub fn from_mpsc(rx: mpsc::Receiver<IndexEvent>) -> Self {
        Self {
            inner: ReceiverInner::Plain(rx),
        }
    }

    /// Receive the next event, retaining its ingress permit until dropped.
    pub async fn recv(&mut self) -> Option<QueuedEvent> {
        match &mut self.inner {
            ReceiverInner::Bounded(rx) => rx.recv().await,
            ReceiverInner::Plain(rx) => rx.recv().await.map(|event| QueuedEvent {
                event,
                _permit: None,
            }),
        }
    }

    /// Receive an already available event without waiting.
    pub fn try_recv(&mut self) -> Result<QueuedEvent, mpsc::error::TryRecvError> {
        match &mut self.inner {
            ReceiverInner::Bounded(rx) => rx.try_recv(),
            ReceiverInner::Plain(rx) => rx.try_recv().map(|event| QueuedEvent {
                event,
                _permit: None,
            }),
        }
    }
}

/// An accepted event. Dropping it releases its pending-event permit.
pub struct QueuedEvent {
    /// The accepted operation.
    pub event: IndexEvent,
    _permit: Option<OwnedSemaphorePermit>,
}
