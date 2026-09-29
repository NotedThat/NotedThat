//! Creating, checking and updating a stream `NotedThat` owns.

use std::time::Duration;

use async_nats::jetstream::Context;
use async_nats::jetstream::stream::{
    Config as StreamConfig, DiscardPolicy, RetentionPolicy, StorageType, Stream,
};

use crate::config::{NatsStorage, NatsStreamSettings};

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
    /// How many messages the stream holds; `None` for no limit.
    pub max_messages: Option<i64>,
    /// Replicas, storage and duplicate window.
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

impl StreamSpec {
    fn config(&self) -> StreamConfig {
        StreamConfig {
            name: self.name.clone(),
            subjects: vec![self.subject.clone()],
            max_age: self.max_age,
            max_messages: self.max_messages.unwrap_or(-1),
            retention: self.retention,
            discard: self.discard,
            storage: storage_type(self.settings.storage),
            num_replicas: self.settings.replicas,
            duplicate_window: self.settings.duplicate_window,
            ..StreamConfig::default()
        }
    }
}

/// Create the stream if absent; otherwise check it is ours and bring the
/// settings `JetStream` can change in place in line with the configuration.
///
/// Changeable in place, and followed: retention age, message limit, replicas
/// and duplicate window. Not changeable, and refused when they differ: the
/// subjects (someone else's stream), the storage type and the retention policy.
/// Everything else on an existing stream is the operator's and is kept as found.
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
    if found.storage != desired.storage {
        return Err(StreamSetupError::Immutable {
            stream: spec.name.clone(),
            field: "storage",
            found: format!("{:?}", found.storage).to_lowercase(),
            wanted: spec.settings.storage.to_string(),
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

    let drifted = found.max_age != desired.max_age
        || found.max_messages != desired.max_messages
        || found.num_replicas != desired.num_replicas
        || found.duplicate_window != desired.duplicate_window;
    if !drifted {
        return Ok(stream);
    }
    // The operator changed a setting; the stream follows the config. Only these
    // fields: the rest of the existing configuration is kept as found.
    let updated = StreamConfig {
        max_age: desired.max_age,
        max_messages: desired.max_messages,
        num_replicas: desired.num_replicas,
        duplicate_window: desired.duplicate_window,
        ..found
    };
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

    #[test]
    fn the_spec_carries_every_shared_setting_into_the_stream_config() {
        let spec = StreamSpec {
            name: "nt".into(),
            name_setting: "NOTEDTHAT_NATS_STREAM",
            subject: "notedthat.events.>".into(),
            retention: RetentionPolicy::Limits,
            discard: DiscardPolicy::Old,
            max_age: Duration::from_secs(60),
            max_messages: None,
            settings: NatsStreamSettings {
                replicas: 3,
                storage: NatsStorage::Memory,
                duplicate_window: Duration::from_secs(30),
            },
        };
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
}
