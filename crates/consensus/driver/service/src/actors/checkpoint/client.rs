//! Checkpoint actor client.

use async_trait::async_trait;
use base_consensus_engine::{
    ForkchoiceCheckpointError, ForkchoiceCheckpointLabel, ForkchoiceCheckpointReader,
};
use base_protocol::L2BlockInfo;
use tokio::sync::{mpsc, oneshot};

use super::CheckpointError;

/// Writes forkchoice checkpoints.
#[async_trait]
pub trait CheckpointWriter: Send + Sync + std::fmt::Debug {
    /// Updates the checkpoint for the requested label.
    async fn update_checkpoint(
        &self,
        label: ForkchoiceCheckpointLabel,
        block: L2BlockInfo,
    ) -> Result<(), CheckpointError>;
}

/// Checkpoint writer that drops all updates.
#[derive(Debug, Default)]
pub struct NoopCheckpointWriter;

#[async_trait]
impl CheckpointWriter for NoopCheckpointWriter {
    async fn update_checkpoint(
        &self,
        _label: ForkchoiceCheckpointLabel,
        _block: L2BlockInfo,
    ) -> Result<(), CheckpointError> {
        Ok(())
    }
}

/// Client used to communicate with the checkpoint actor.
#[derive(Debug, Clone)]
pub struct CheckpointClient {
    request_tx: mpsc::Sender<CheckpointRequest>,
}

impl CheckpointClient {
    /// Creates a new checkpoint client.
    pub const fn new(request_tx: mpsc::Sender<CheckpointRequest>) -> Self {
        Self { request_tx }
    }

    async fn send(&self, request: CheckpointRequest) -> Result<(), CheckpointError> {
        self.request_tx.send(request).await.map_err(|_| CheckpointError::ChannelClosed)
    }
}

/// Request sent to the checkpoint actor.
#[derive(Debug)]
pub enum CheckpointRequest {
    /// Read the checkpoint for a label.
    Read {
        /// The label to read.
        label: ForkchoiceCheckpointLabel,
        /// Response channel.
        response_tx: oneshot::Sender<Result<Option<L2BlockInfo>, CheckpointError>>,
    },
    /// Write the checkpoint for a label.
    Write {
        /// The label to write.
        label: ForkchoiceCheckpointLabel,
        /// The checkpoint block.
        block: L2BlockInfo,
        /// Response channel.
        response_tx: oneshot::Sender<Result<(), CheckpointError>>,
    },
}

#[async_trait]
impl ForkchoiceCheckpointReader for CheckpointClient {
    async fn checkpoint(
        &self,
        label: ForkchoiceCheckpointLabel,
    ) -> Result<Option<L2BlockInfo>, ForkchoiceCheckpointError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.send(CheckpointRequest::Read { label, response_tx })
            .await
            .map_err(|e| ForkchoiceCheckpointError::Unavailable(e.to_string()))?;
        response_rx
            .await
            .map_err(|_| {
                ForkchoiceCheckpointError::Unavailable(CheckpointError::ResponseDropped.to_string())
            })?
            .map_err(|e| ForkchoiceCheckpointError::Unavailable(e.to_string()))
    }
}

#[async_trait]
impl CheckpointWriter for CheckpointClient {
    async fn update_checkpoint(
        &self,
        label: ForkchoiceCheckpointLabel,
        block: L2BlockInfo,
    ) -> Result<(), CheckpointError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.send(CheckpointRequest::Write { label, block, response_tx }).await?;
        response_rx.await.map_err(|_| CheckpointError::ResponseDropped)?
    }
}
