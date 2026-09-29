//! Creating, checking and updating a stream `NotedThat` owns.

use std::time::Duration;

use async_nats::jetstream::Context;
use async_nats::jetstream::stream::{
    Config as StreamConfig, DiscardPolicy, RetentionPolicy, StorageType, Stream,
};

use crate::config::{
    DEFAULT_NATS_DUPLICATE_WINDOW_SECS, DEFAULT_NATS_REPLICAS, NatsStorage, NatsStreamSettings,
};

/// What a NotedThat-owned stream must look like.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    /// The stream name.
    pub name: String,
    /// The environment variable that names it, for diagnostics.
    pub name_setting: &'static str,
    /// The one subject filter the stream captures, e.g. `notedthat.events.>`.
    pub subject: String,
    /// Limits for a log, work-queue for a queue. Never changed on an existing stream.
    pub retention: RetentionPolicy,
    /// What happens at a limit: drop the oldest (a log) or refuse the newest (a queue).
    pub discard: DiscardPolicy,
    /// How long a message is kept; zero for no limit.
    pub max_age: Duration,
    /// How many messages the stream holds; `None` creates the stream without a
    /// limit and leaves an existing stream's limit as found.
    pub max_messages: Option<i64>,
    /// Replicas, storage and duplicate window; each `None` is a default on
    /// create and left as found on an existing stream.
    pub settings: NatsStreamSettings,
}

/// Errors from bringing a stream up.
#[derive(Debug, thiserror::Error)]
pub enum StreamSetupError {
    /// The stream could not be created, read or updated.
    #[error("could not open JetStream stream {stream}: {message}")]
    Stream {
        /// The configured stream name.
        stream: String,
        /// What the server said.
        message: String,
    },
    /// A stream by the configured name exists but is not ours.
    #[error(
        "JetStream stream {stream} exists with subjects {subjects:?}, not [\"{expected}\"]; \
         point {setting} at a stream NotedThat owns"
    )]
    Mismatch {
        /// The configured stream name.
        stream: String,
        /// The setting naming the stream, both forms.
        setting: String,
        /// The subjects the existing stream captures.
        subjects: Vec<String>,
        /// The subject filter this stream needs.
        expected: String,
    },
    /// A setting `JetStream` cannot change on an existing stream differs.
    #[error(
        "JetStream stream {stream} has {field} {found}, but the configuration asks for \
         {wanted}; JetStream cannot change it in place — delete the stream or restore the setting"
    )]
    Immutable {
        /// The configured stream name.
        stream: String,
        /// Which setting.
        field: &'static str,
        /// What the stream has.
        found: String,
        /// What the configuration asks for.
        wanted: String,
    },
}

fn storage_type(storage: NatsStorage) -> StorageType {
    match storage {
        NatsStorage::File => StorageType::File,
        NatsStorage::Memory => StorageType::Memory,
    }
}

/// `JetStream` refuses a duplicate window longer than the retention age; zero is
/// no age limit.
fn within_max_age(window: Duration, max_age: Duration) -> Duration {
    if max_age.is_zero() {
        window
    } else {
        window.min(max_age)
    }
}

impl StreamSpec {
    /// The configuration of a stream created from this spec.
    fn config(&self) -> StreamConfig {
        StreamConfig {
            name: self.name.clone(),
            subjects: vec![self.subject.clone()],
            max_age: self.max_age,
            max_messages: self.max_messages.unwrap_or(-1),
            retention: self.retention,
            discard: self.discard,
            storage: storage_type(self.settings.storage.unwrap_or_default()),
            num_replicas: self.settings.replicas.unwrap_or(DEFAULT_NATS_REPLICAS),
            duplicate_window: self.settings.duplicate_window.unwrap_or_else(|| {
                within_max_age(
                    Duration::from_secs(DEFAULT_NATS_DUPLICATE_WINDOW_SECS),
                    self.max_age,
                )
            }),
            ..StreamConfig::default()
        }
    }

    /// `found` with what this spec configures applied; everything it leaves
    /// unset is kept as found.
    fn apply_to(&self, found: &StreamConfig) -> StreamConfig {
        let mut updated = found.clone();
        updated.max_age = self.max_age;
        if let Some(max_messages) = self.max_messages {
            updated.max_messages = max_messages;
        }
        if let Some(replicas) = self.settings.replicas {
            updated.num_replicas = replicas;
        }
        updated.duplicate_window = match self.settings.duplicate_window {
            Some(window) => window,
            // A window nobody configured still has to fit a shortened retention,
            // or the server refuses the update.
            None => within_max_age(found.duplicate_window, self.max_age),
        };
        updated
    }
}

/// Create the stream if absent; otherwise check it is ours and bring the
/// settings `JetStream` can change in place in line with the configuration.
///
/// Changeable in place, and followed when configured: retention age, message
/// limit, replicas and duplicate window. Not changeable, and refused when they
/// differ: the subjects (someone else's stream), the retention policy and a
/// configured storage type. A setting left unconfigured is kept as found, and
/// so is everything else on an existing stream: it is the operator's.
///
/// # Errors
///
/// The stream cannot be created, read or updated, or it exists with different
/// subjects, storage or retention policy.
pub async fn ensure_stream(js: &Context, spec: &StreamSpec) -> Result<Stream, StreamSetupError> {
    let desired = spec.config();
    let stream_error = |message: String| StreamSetupError::Stream {
        stream: spec.name.clone(),
        message,
    };
    let stream = js
        .get_or_create_stream(desired.clone())
        .await
        .map_err(|error| stream_error(error.to_string()))?;
    let found = stream.cached_info().config.clone();

    if found.subjects != desired.subjects {
        return Err(StreamSetupError::Mismatch {
            stream: spec.name.clone(),
            setting: notedthat_core::setting(spec.name_setting),
            subjects: found.subjects,
            expected: spec.subject.clone(),
        });
    }
    if let Some(storage) = spec.settings.storage
        && found.storage != storage_type(storage)
    {
        return Err(StreamSetupError::Immutable {
            stream: spec.name.clone(),
            field: "storage",
            found: format!("{:?}", found.storage).to_lowercase(),
            wanted: storage.to_string(),
        });
    }
    if found.retention != desired.retention {
        return Err(StreamSetupError::Immutable {
            stream: spec.name.clone(),
            field: "retention",
            found: format!("{:?}", found.retention),
            wanted: format!("{:?}", desired.retention),
        });
    }

    // The operator changed a setting; the stream follows the config. Only the
    // configured fields: the rest of the existing configuration is kept as found.
    let updated = spec.apply_to(&found);
    if updated == found {
        return Ok(stream);
    }
    js.update_stream(&updated)
        .await
        .map_err(|error| stream_error(format!("could not update the stream: {error}")))?;
    js.get_stream(&spec.name)
        .await
        .map_err(|error| stream_error(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(max_age: Duration, settings: NatsStreamSettings) -> StreamSpec {
        StreamSpec {
            name: "nt".into(),
            name_setting: "NOTEDTHAT_NATS_STREAM",
            subject: "notedthat.events.>".into(),
            retention: RetentionPolicy::Limits,
            discard: DiscardPolicy::Old,
            max_age,
            max_messages: None,
            settings,
        }
    }

    #[test]
    fn the_spec_carries_every_shared_setting_into_the_stream_config() {
        let spec = spec(
            Duration::from_secs(60),
            NatsStreamSettings {
                replicas: Some(3),
                storage: Some(NatsStorage::Memory),
                duplicate_window: Some(Duration::from_secs(30)),
            },
        );
        let config = spec.config();
        assert_eq!(config.subjects, vec!["notedthat.events.>".to_string()]);
        assert_eq!(config.num_replicas, 3);
        assert_eq!(config.storage, StorageType::Memory);
        assert_eq!(config.duplicate_window, Duration::from_secs(30));
        assert_eq!(config.max_messages, -1);

        let bounded = StreamSpec {
            max_messages: Some(1024),
            ..spec
        }
        .config();
        assert_eq!(bounded.max_messages, 1024);
    }

    #[test]
    fn unset_settings_create_with_defaults_and_the_window_fits_the_retention() {
        let config = spec(Duration::from_secs(600), NatsStreamSettings::default()).config();
        assert_eq!(config.num_replicas, 1);
        assert_eq!(config.storage, StorageType::File);
        assert_eq!(config.duplicate_window, Duration::from_secs(120));

        // nats-server refuses a window longer than max_age (error 10052).
        let short = spec(Duration::from_secs(60), NatsStreamSettings::default()).config();
        assert_eq!(short.duplicate_window, Duration::from_secs(60));
    }

    #[test]
    fn an_existing_stream_keeps_what_is_not_configured() {
        let found = StreamConfig {
            max_age: Duration::from_secs(600),
            max_messages: 5_000,
            num_replicas: 3,
            duplicate_window: Duration::from_secs(90),
            ..spec(Duration::from_secs(600), NatsStreamSettings::default()).config()
        };

        let unset = spec(Duration::from_secs(600), NatsStreamSettings::default());
        assert_eq!(
            unset.apply_to(&found),
            found,
            "nothing configured, nothing changes"
        );

        let configured = StreamSpec {
            max_messages: Some(10),
            ..spec(
                Duration::from_mins(15),
                NatsStreamSettings {
                    replicas: Some(1),
                    storage: None,
                    duplicate_window: Some(Duration::from_secs(30)),
                },
            )
        };
        let updated = configured.apply_to(&found);
        assert_eq!(updated.max_age, Duration::from_mins(15));
        assert_eq!(updated.max_messages, 10);
        assert_eq!(updated.num_replicas, 1);
        assert_eq!(updated.duplicate_window, Duration::from_secs(30));

        // A shortened retention takes an unconfigured window down with it.
        let shortened = spec(Duration::from_secs(60), NatsStreamSettings::default());
        assert_eq!(
            shortened.apply_to(&found).duplicate_window,
            Duration::from_secs(60)
        );
    }
}
