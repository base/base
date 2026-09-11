//! Stores engine API messages to disk for later inspection and replay.

use std::{
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll, ready},
    time::SystemTime,
};

use base_common_io as fs;
use base_common_types_payload::{
    BasePayloadBuilderAttributes, BeaconEngineMessage, ForkchoiceState,
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::*;

/// A message from the engine API that has been stored to disk.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StoredEngineApiMessage {
    /// The on-disk representation of an `engine_forkchoiceUpdated` method call.
    ForkchoiceUpdated {
        /// The [`ForkchoiceState`] sent in the persisted call.
        state: ForkchoiceState,
        /// The payload attributes sent in the persisted call, if any.
        payload_attrs: Option<BasePayloadBuilderAttributes>,
    },
    /// The on-disk representation of an `engine_newPayload` method call.
    NewPayload {
        /// The [`base_common_types_payload::ExecutionData`] sent in the persisted call.
        #[serde(flatten)]
        payload: base_common_types_payload::ExecutionData,
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

    /// Stores the received [`BeaconEngineMessage`] to disk, appending the `received_at` time to the
    /// path.
    pub fn on_message(
        &self,
        msg: &BeaconEngineMessage,
        received_at: SystemTime,
    ) -> eyre::Result<()> {
        fs::Files::create_dir_all(&self.path)?; // ensure that store path had been created
        let timestamp = received_at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();
        match msg {
            BeaconEngineMessage::ForkchoiceUpdated { state, payload_attrs, tx: _tx } => {
                let filename = format!("{}-fcu-{}.json", timestamp, state.head_block_hash);
                fs::Files::write(
                    self.path.join(filename),
                    serde_json::to_vec(&StoredEngineApiMessage::ForkchoiceUpdated {
                        state: *state,
                        payload_attrs: payload_attrs.clone(),
                    })?,
                )?;
            }
            BeaconEngineMessage::NewPayload { payload, .. } => {
                let filename = format!("{}-new_payload-{}.json", timestamp, payload.block_hash());
                fs::Files::write(
                    self.path.join(filename),
                    serde_json::to_vec(&StoredEngineApiMessage::NewPayload {
                        payload: payload.clone(),
                    })?,
                )?;
            }
        };
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
    S: Stream<Item = BeaconEngineMessage>,
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
