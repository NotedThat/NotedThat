//! The NATS connection every `NotedThat` `JetStream` user shares (D75).
//!
//! NATS is optional and chosen at runtime: nothing here runs unless an events
//! backend or the indexing queue selects it. When one does, the process opens
//! one connection with [`connect`] — URL, authentication, TLS — and each user
//! brings its own stream up with [`ensure_stream`], under the replica, storage
//! and deduplication settings every NotedThat-owned stream shares.
#![deny(missing_docs)]

pub mod config;
pub mod connect;
pub mod stream;

pub use config::{
    DEFAULT_NATS_DUPLICATE_WINDOW_SECS, DEFAULT_NATS_REPLICAS, NATS_CONNECT_ENV_VARS, NatsAuth,
    NatsConnectConfig, NatsConnectSettings, NatsStorage, NatsStreamSettings, NatsTls,
    parse_positive, parse_stream_name,
};
pub use connect::{NatsConnectError, TIMEOUT, connect};
pub use stream::{StreamSetupError, StreamSpec, ensure_stream};

/// The header naming a message's schema and version, on every message `NotedThat`
/// publishes, e.g. `NotedThat-Schema: object-event/1`.
pub const SCHEMA_HEADER: &str = "NotedThat-Schema";
