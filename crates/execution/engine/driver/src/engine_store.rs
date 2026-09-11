//! Stores engine API messages to disk for later inspection and replay.

use std::{
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll, ready},
    time::SystemTime,
};

use base_common_io as fs;
use base_common_types_payload::{BasePayloadBuilderAttributes, ExecutionCommand, ForkchoiceState};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::error;

/// Versioned recording of a native execution command.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "version", content = "command")]
pub enum StoredEngineApiMessage {
    /// Native domain commands; legacy recordings are intentionally unsupported.
    #[serde(rename = "2")]
    V2(StoredExecutionCommand),
}

/// Serializable inputs to an execution command.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum StoredExecutionCommand {
    /// Import and canonicalize a block.
    AppendPayload {
        /// Payload to execute.
        payload: Box<base_common_types_payload::ExecutionData>,
        /// Requested heads.
        heads: ForkchoiceState,
        /// Whether fault injection skips import.
        skip_import: bool,
        /// Whether fault injection skips head application.
        skip_heads: bool,
    },
    /// Start a build on the selected parent.
    StartBuilding {
        /// Requested heads.
        heads: ForkchoiceState,
        /// Build attributes.
        attributes: Box<BasePayloadBuilderAttributes>,
    },
    /// Apply heads without starting a build.
    UpdateHeads {
        /// Requested heads.
        heads: ForkchoiceState,
    },
}

/// This can read and write engine API messages in a specific directory.
#[derive(Debug)]
pub struct EngineMessageStore {
    /// The path to the directory that stores the engine API messages.
    path: PathBuf,
}

impl EngineMessageStore {
    /// Creates a new [`EngineMessageStore`] at the given path.
    ///
    /// The path is expected to be a directory, where individual message JSON files will be stored.
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Stores the received [`ExecutionCommand`] to disk, appending the `received_at` time to the
    /// path.
    pub fn on_message(&self, msg: &ExecutionCommand, received_at: SystemTime) -> eyre::Result<()> {
        fs::Files::create_dir_all(&self.path)?; // ensure that store path had been created
        let timestamp = received_at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let (hash, command) = match msg {
            ExecutionCommand::AppendPayload { payload, heads, skip_import, skip_heads, .. } => (
                heads.head_block_hash,
                StoredExecutionCommand::AppendPayload {
                    payload: payload.clone(),
                    heads: *heads,
                    skip_import: *skip_import,
                    skip_heads: *skip_heads,
                },
            ),
            ExecutionCommand::StartBuilding { heads, attributes, .. } => (
                heads.head_block_hash,
                StoredExecutionCommand::StartBuilding {
                    heads: *heads,
                    attributes: attributes.clone(),
                },
            ),
            ExecutionCommand::UpdateHeads { heads, .. } => {
                (heads.head_block_hash, StoredExecutionCommand::UpdateHeads { heads: *heads })
            }
        };
        let operation = match msg {
            ExecutionCommand::AppendPayload { .. } => "append",
            ExecutionCommand::StartBuilding { .. } => "start-building",
            ExecutionCommand::UpdateHeads { .. } => "update-heads",
        };
        fs::Files::write(
            self.path.join(format!("{timestamp}-{operation}-{hash}.json")),
            serde_json::to_vec(&StoredEngineApiMessage::V2(command))?,
        )?;
        Ok(())
    }
}

/// A wrapper stream that stores Engine API messages in
/// the specified directory.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct EngineStoreStream<S> {
    /// Inner message stream.
    #[pin]
    stream: S,
    /// Engine message store.
    store: EngineMessageStore,
}

impl<S> EngineStoreStream<S> {
    /// Create new engine store stream wrapper.
    pub const fn new(stream: S, path: PathBuf) -> Self {
        Self { stream, store: EngineMessageStore::new(path) }
    }
}

impl<S> Stream for EngineStoreStream<S>
where
    S: Stream<Item = ExecutionCommand>,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let next = ready!(this.stream.poll_next_unpin(cx));
        if let Some(msg) = &next
            && let Err(error) = this.store.on_message(msg, SystemTime::now())
        {
            error!(target: "engine::stream::store", ?msg, %error, "Error handling Engine API message");
        }
        Poll::Ready(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_recordings_round_trip_and_legacy_recordings_are_rejected() {
        let recording = StoredEngineApiMessage::V2(StoredExecutionCommand::UpdateHeads {
            heads: ForkchoiceState::default(),
        });
        let json = serde_json::to_vec(&recording).unwrap();
        assert!(matches!(
            serde_json::from_slice::<StoredEngineApiMessage>(&json).unwrap(),
            StoredEngineApiMessage::V2(StoredExecutionCommand::UpdateHeads { .. })
        ));
        let legacy = serde_json::json!({ "forkchoiceUpdated": { "state": ForkchoiceState::default(), "payload_attrs": null } });
        assert!(serde_json::from_value::<StoredEngineApiMessage>(legacy).is_err());
    }
}
