//! Forkchoice checkpoint interfaces for sync start recovery.

use std::fmt;

use async_trait::async_trait;
use base_protocol::L2BlockInfo;
use thiserror::Error;

/// Forkchoice labels that may be recovered from a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkchoiceCheckpointLabel {
    /// The safe L2 head.
    Safe,
    /// The finalized L2 head.
    Finalized,
}

impl ForkchoiceCheckpointLabel {
    /// Returns the label as a static string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Finalized => "finalized",
        }
    }
}

impl fmt::Display for ForkchoiceCheckpointLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when reading forkchoice checkpoints.
#[derive(Debug, Error)]
pub enum ForkchoiceCheckpointError {
    /// The checkpoint reader is unavailable.
    #[error("forkchoice checkpoint reader unavailable: {0}")]
    Unavailable(String),
}

/// Reads forkchoice checkpoints.
#[async_trait]
pub trait ForkchoiceCheckpointReader: Send + Sync + std::fmt::Debug {
    /// Returns the checkpoint for the requested label, if present.
    async fn checkpoint(
        &self,
        label: ForkchoiceCheckpointLabel,
    ) -> Result<Option<L2BlockInfo>, ForkchoiceCheckpointError>;
}

/// Checkpoint reader that never returns checkpoints.
#[derive(Debug, Default)]
pub struct NoopForkchoiceCheckpointReader;

#[async_trait]
impl ForkchoiceCheckpointReader for NoopForkchoiceCheckpointReader {
    async fn checkpoint(
        &self,
        _label: ForkchoiceCheckpointLabel,
    ) -> Result<Option<L2BlockInfo>, ForkchoiceCheckpointError> {
        Ok(None)
    }
}
