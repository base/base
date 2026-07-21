//! Error types for the re-anchor task.

use alloy_rpc_types_engine::PayloadStatusEnum;
use alloy_transport::{RpcError, TransportErrorKind};
use base_protocol::FromBlockError;

use crate::{
    EngineTaskError, SynchronizeTaskError, task_queue::tasks::task::EngineTaskErrorSeverity,
};

/// An error that occurs when re-anchoring the unsafe head.
#[derive(Debug, thiserror::Error)]
pub enum ReanchorTaskError {
    /// Failed to insert new payload.
    #[error("Failed to insert new payload: {0}")]
    InsertFailed(RpcError<TransportErrorKind>),
    /// Unexpected payload status.
    #[error("Unexpected payload status: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
    /// Error converting the payload + chain genesis into an L2 block info.
    #[error(transparent)]
    L2BlockInfoConstruction(#[from] FromBlockError),
    /// The forkchoice update call to canonicalize the payload failed.
    #[error(transparent)]
    ForkchoiceUpdateFailed(#[from] SynchronizeTaskError),
    /// The forkchoice update completed without advancing the unsafe head to the payload.
    #[error("Forkchoice update did not advance to the re-anchored payload")]
    ForkchoiceUpdateDidNotAdvance,
}

impl EngineTaskError for ReanchorTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::InsertFailed(_)
            | Self::UnexpectedPayloadStatus(_)
            | Self::ForkchoiceUpdateDidNotAdvance => EngineTaskErrorSeverity::Temporary,
            Self::L2BlockInfoConstruction(_) => EngineTaskErrorSeverity::Critical,
            Self::ForkchoiceUpdateFailed(inner) => inner.severity(),
        }
    }
}
