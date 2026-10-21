//! Contains the error types for the [`InsertTask`].
//!
//! [InsertTask]: crate::InsertTask

use alloy_rpc_types_engine::PayloadStatusEnum;
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_rpc_types_engine::BasePayloadError;
use base_protocol::{BaseTimeScheduleError, FromBlockError};

use crate::{
    EngineTaskError, SynchronizeTaskError, task_queue::tasks::task::EngineTaskErrorSeverity,
};

/// An error that occurs when running the [`InsertTask`].
///
/// [InsertTask]: crate::InsertTask
#[derive(Debug, thiserror::Error)]
pub enum InsertTaskError {
    /// No payloads were provided for authoritative insertion.
    #[error("Authoritative insertion requires at least one payload")]
    EmptyAuthoritativePayloads,
    /// Error converting a payload into a block.
    #[error(transparent)]
    FromBlockError(#[from] BasePayloadError),
    /// Failed to insert new payload.
    #[error("Failed to insert new payload: {0}")]
    InsertFailed(RpcError<TransportErrorKind>),
    /// Unexpected payload status
    #[error("Unexpected payload status: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
    /// Error converting the payload + chain genesis into an L2 block info.
    #[error(transparent)]
    L2BlockInfoConstruction(#[from] FromBlockError),
    /// The payload timestamp does not match the absolute rollup schedule.
    #[error(transparent)]
    InvalidBaseTimeSchedule(#[from] BaseTimeScheduleError),
    /// The forkchoice update call to consolidate the block into the engine state failed.
    #[error(transparent)]
    ForkchoiceUpdateFailed(#[from] SynchronizeTaskError),
    /// The forkchoice update completed without applying the inserted payload.
    #[error("Forkchoice update did not apply the inserted payload")]
    ForkchoiceUpdateDidNotApply,
}

impl EngineTaskError for InsertTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::EmptyAuthoritativePayloads
            | Self::FromBlockError(_)
            | Self::L2BlockInfoConstruction(_)
            | Self::InvalidBaseTimeSchedule(_) => EngineTaskErrorSeverity::Critical,
            Self::InsertFailed(_)
            | Self::UnexpectedPayloadStatus(_)
            | Self::ForkchoiceUpdateDidNotApply => EngineTaskErrorSeverity::Temporary,
            Self::ForkchoiceUpdateFailed(inner) => inner.severity(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InsertTaskError;
    use crate::{EngineTaskError, EngineTaskErrorSeverity};

    #[test]
    fn empty_authoritative_payloads_is_not_retryable() {
        assert_eq!(
            InsertTaskError::EmptyAuthoritativePayloads.severity(),
            EngineTaskErrorSeverity::Critical
        );
    }
}
