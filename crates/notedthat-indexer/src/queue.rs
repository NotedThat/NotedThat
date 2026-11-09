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
            permits,
            capacity,
        },
        IndexQueueReceiver { rx },
    )
}

/// Cloneable producer handle for the indexing ingress.
#[derive(Clone)]
pub struct IndexQueueSender {
    tx: mpsc::UnboundedSender<QueuedEvent>,
    permits: Arc<Semaphore>,
    capacity: usize,
}

impl IndexQueueSender {
    /// Try to accept an event without waiting for room.
    ///
    /// # Errors
    ///
    /// Returns [`TrySendError::Full`](mpsc::error::TrySendError::Full) when the
    /// pending-event bound is reached, and
    /// [`TrySendError::Closed`](mpsc::error::TrySendError::Closed) when the receiver is
    /// gone; both hand the event back.
    pub fn try_send(&self, event: IndexEvent) -> Result<(), mpsc::error::TrySendError<IndexEvent>> {
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
                _permit: permit,
            })
            .map_err(|error| mpsc::error::TrySendError::Closed(error.0.event))
    }

    /// Wait until the pending-event bound has room, then accept an event.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`](mpsc::error::SendError), handing the event back, when the
    /// receiver is gone.
    pub async fn send(&self, event: IndexEvent) -> Result<(), mpsc::error::SendError<IndexEvent>> {
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            return Err(mpsc::error::SendError(event));
        };
        self.tx
            .send(QueuedEvent {
                event,
                _permit: permit,
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
        self.permits.available_permits()
    }
}

/// Receiver owned by the index scheduler.
///
/// It is only ever built by [`index_queue`], so every event it yields carries
/// the permit that bounds the worker's lanes (#257).
#[derive(Debug)]
pub struct IndexQueueReceiver {
    rx: mpsc::UnboundedReceiver<QueuedEvent>,
}

impl IndexQueueReceiver {
    /// Receive the next event, retaining its ingress permit until dropped.
    pub async fn recv(&mut self) -> Option<QueuedEvent> {
        self.rx.recv().await
    }

    /// Receive an already available event without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`TryRecvError::Empty`](mpsc::error::TryRecvError::Empty) when no event is
    /// waiting, and [`TryRecvError::Disconnected`](mpsc::error::TryRecvError::Disconnected)
    /// when every sender is gone.
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
