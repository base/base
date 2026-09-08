//! Contains error types for the [`crate::FinalizeTask`].

use base_protocol::FromBlockError;
use thiserror::Error;

use crate::{
    EngineTaskError, SynchronizeTaskError, task_queue::tasks::task::EngineTaskErrorSeverity,
};

/// An error that occurs when running the [`crate::FinalizeTask`].
#[derive(Debug, Error)]
pub enum FinalizeTaskError {
    /// The block is not safe, and therefore cannot be finalized.
    #[error("Attempted to finalize a block that is not yet safe")]
    BlockNotSafe,
    /// The block to finalize was not found.
    #[error("The block to finalize was not found: Number {0}")]
    BlockNotFound(u64),
    /// An error occurred while decoding the native block into [`L2BlockInfo`].
    ///
    /// [`L2BlockInfo`]: base_protocol::L2BlockInfo
    #[error(transparent)]
    FromBlock(#[from] FromBlockError),
    /// A temporary local read failure.
    #[error(transparent)]
    Local(#[from] crate::EngineClientError),
    /// The forkchoice update call to finalize the block failed.
    #[error(transparent)]
    ForkchoiceUpdateFailed(#[from] SynchronizeTaskError),
}

impl EngineTaskError for FinalizeTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::BlockNotSafe | Self::BlockNotFound(_) | Self::FromBlock(_) => {
                EngineTaskErrorSeverity::Critical
            }
            Self::Local(_) => EngineTaskErrorSeverity::Temporary,
            Self::ForkchoiceUpdateFailed(inner) => inner.severity(),
        }
    }
}
