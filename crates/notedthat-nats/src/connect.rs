//! Opening the one NATS connection a process shares.

use std::time::Duration;

use async_nats::{Client, ConnectOptions, Event};
use notedthat_core::metrics::name;

use crate::config::{NatsAuth, NatsConnectConfig};

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
    let mut options = ConnectOptions::new()
        .connection_timeout(TIMEOUT)
        .request_timeout(Some(TIMEOUT))
        .name(name)
        .event_callback(|event| async move { record(&event) });

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

    let client = options.connect(&config.url).await?;
    metrics::gauge!(name::NATS_CONNECTED).set(1.0);
    Ok(client)
}

/// Mirror a connection state change into the metrics.
///
/// The initial connect is not an event the callback sees before `connect`
/// returns, so every `Connected` here is a reconnect.
fn record(event: &Event) {
    match event {
        Event::Connected => {
            metrics::gauge!(name::NATS_CONNECTED).set(1.0);
            metrics::counter!(name::NATS_RECONNECTS).increment(1);
            tracing::info!(target: "notedthat::nats", "NATS_RECONNECTED");
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
