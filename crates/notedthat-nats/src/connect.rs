//! Opening the one NATS connection a process shares.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_nats::{Client, ConnectOptions, Event};
use notedthat_core::metrics::name;

use crate::config::{NatsAuth, NatsConnectConfig, UrlCredentials, split_url_credentials};

/// How long a connect, a `JetStream` API call or a publish acknowledgement may
/// take before it is a failure.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from opening the connection.
#[derive(Debug, thiserror::Error)]
pub enum NatsConnectError {
    /// A credentials, seed or certificate file could not be read.
    #[error("could not read {setting}: {message}")]
    File {
        /// The setting naming the file, both forms.
        setting: String,
        /// What the filesystem said.
        message: String,
    },
    /// The server could not be reached, or refused the connection.
    #[error("could not connect to NATS: {0}")]
    Connect(#[from] async_nats::ConnectError),
}

/// Connect with every configured option.
///
/// The client reconnects on its own for the life of the process; each state
/// change is reflected in `notedthat_nats_connected` and every re-established
/// connection counts in `notedthat_nats_reconnects_total`.
///
/// # Errors
///
/// A configured file cannot be read, or the server cannot be reached (or
/// refuses the credentials) within [`TIMEOUT`].
pub async fn connect(config: &NatsConnectConfig, name: &str) -> Result<Client, NatsConnectError> {
    let events = Arc::new(ConnectionEvents::default());
    let mut options = ConnectOptions::new()
        .connection_timeout(TIMEOUT)
        .request_timeout(Some(TIMEOUT))
        .name(name)
        .event_callback(move |event| {
            let events = Arc::clone(&events);
            async move { events.record(&event) }
        });

    // async-nats parses `user:pass@` in the URL but never sends it.
    let url = match split_url_credentials(&config.url) {
        Some((UrlCredentials::UserPassword(user, password), url)) => {
            options = options.user_and_password(user, password);
            url
        }
        Some((UrlCredentials::Token(token), url)) => {
            options = options.token(token);
            url
        }
        None => config.url.clone(),
    };

    options = match &config.auth {
        NatsAuth::None => options,
        NatsAuth::CredsFile(path) => {
            options
                .credentials_file(path)
                .await
                .map_err(|error| NatsConnectError::File {
                    setting: notedthat_core::setting("NOTEDTHAT_NATS_CREDS_FILE"),
                    message: error.to_string(),
                })?
        }
        NatsAuth::NkeySeedFile(path) => {
            let seed =
                tokio::fs::read_to_string(path)
                    .await
                    .map_err(|error| NatsConnectError::File {
                        setting: notedthat_core::setting("NOTEDTHAT_NATS_NKEY_SEED_FILE"),
                        message: error.to_string(),
                    })?;
            options.nkey(seed.trim().to_string())
        }
        NatsAuth::Token(token) => options.token(token.clone()),
    };

    if let Some(ca) = &config.tls.ca_file {
        options = options.add_root_certificates(ca.clone());
    }
    if let Some((cert, key)) = &config.tls.client_cert {
        options = options.add_client_certificate(cert.clone(), key.clone());
    }
    if config.tls.required {
        options = options.require_tls(true);
    }

    let client = options.connect(url).await?;
    metrics::gauge!(name::NATS_CONNECTED).set(1.0);
    Ok(client)
}

/// Mirrors one connection's state changes into the metrics.
#[derive(Debug, Default)]
struct ConnectionEvents {
    /// Whether a `Connected` has been seen: async-nats reports the initial
    /// connect as `Connected` too, and only the ones after it are reconnects.
    connected_before: AtomicBool,
}

impl ConnectionEvents {
    fn record(&self, event: &Event) {
        match event {
            Event::Connected => {
                metrics::gauge!(name::NATS_CONNECTED).set(1.0);
                if self.connected_before.swap(true, Ordering::Relaxed) {
                    metrics::counter!(name::NATS_RECONNECTS).increment(1);
                    tracing::info!(target: "notedthat::nats", "NATS_RECONNECTED");
                }
            }
            Event::Disconnected | Event::Closed => {
                metrics::gauge!(name::NATS_CONNECTED).set(0.0);
                tracing::warn!(target: "notedthat::nats", event = %event, "NATS_DISCONNECTED");
            }
            other => {
                tracing::debug!(target: "notedthat::nats", event = %other, "NATS connection event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::*;

    #[test]
    fn the_initial_connect_is_not_a_reconnect() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let events = ConnectionEvents::default();
            events.record(&Event::Connected);
            events.record(&Event::Disconnected);
            events.record(&Event::Connected);
        });

        let reconnects: Vec<DebugValue> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, ..)| key.key().name() == name::NATS_RECONNECTS)
            .map(|(.., value)| value)
            .collect();
        assert_eq!(reconnects, vec![DebugValue::Counter(1)]);
    }
}
