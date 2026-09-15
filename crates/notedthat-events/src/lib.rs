//! Event log adapters behind [`notedthat_core::EventPublisher`].
//!
//! `memory` is a process-local ring: replay across reconnects, nothing across
//! restarts or replicas. `nats` is a `JetStream` stream shared by every replica,
//! with the stream sequence as the event id. Which one runs is
//! `notedthat-server`'s `NOTEDTHAT_EVENTS_BACKEND` selector; this crate only
//! knows how to build and drive each.
#![deny(missing_docs)]

pub mod config;
pub mod memory;
#[cfg(feature = "nats")]
pub mod nats;

pub use config::{
    DEFAULT_MEMORY_CAPACITY, DEFAULT_NATS_MAX_AGE_SECS, DEFAULT_NATS_STREAM, MEMORY_ENV_VARS,
    MemoryConfig, MemorySettings, NATS_ENV_VARS, NatsConfig, NatsSettings,
};
pub use memory::MemoryPublisher;
#[cfg(feature = "nats")]
pub use nats::NatsPublisher;
