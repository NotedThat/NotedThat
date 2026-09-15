//! Bringing up the object change event log `NOTEDTHAT_EVENTS_BACKEND` selects.
//!
//! The one place at startup that reaches a broker: `backends_from_config` is
//! connectionless by design, and a `nats` deployment must refuse to start
//! rather than run without its log (D39, D55).

use std::sync::Arc;

use anyhow::Context;
use notedthat_core::EventPublisher;
use notedthat_events::MemoryPublisher;
use tracing::info;

use crate::config::EventsConfig;

/// Build, and for a broker connect, the configured event log. `None` when no
/// backend is selected.
///
/// # Errors
///
/// The broker cannot be reached, or holds a stream by the configured name
/// whose subjects are not this server's.
pub(super) async fn connect(
    config: &EventsConfig,
) -> anyhow::Result<Option<Arc<dyn EventPublisher>>> {
    let publisher: Arc<dyn EventPublisher> = match config {
        EventsConfig::None => {
            info!(
                backend = "none",
                "events backend selected; changes are not announced"
            );
            return Ok(None);
        }
        EventsConfig::Memory(memory) => {
            info!(
                backend = "memory",
                capacity = memory.capacity,
                "events backend selected"
            );
            Arc::new(MemoryPublisher::new(memory.capacity))
        }
        EventsConfig::Nats(nats) => {
            let publisher = notedthat_events::NatsPublisher::connect(nats)
                .await
                .context("failed to reach NOTEDTHAT_NATS_URL (--nats-url)")?;
            info!(
                backend = "nats",
                stream = %nats.stream,
                max_age_secs = nats.max_age.as_secs(),
                "events backend selected"
            );
            Arc::new(publisher)
        }
    };
    Ok(Some(publisher))
}
