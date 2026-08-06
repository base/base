//! Contains error types for the [`crate::SynchronizeTask`].

use alloy_rpc_types_engine::PayloadStatusEnum;
use alloy_transport::{RpcError, TransportErrorKind};
use thiserror::Error;

use crate::{EngineTaskError, task_queue::tasks::task::EngineTaskErrorSeverity};

/// An error that occurs when running the [`crate::SynchronizeTask`].
#[derive(Debug, Error)]
pub enum SynchronizeTaskError {
    /// The forkchoice update call to the engine api failed.
    #[error("Forkchoice update engine api call failed due to an RPC error: {0}")]
    ForkchoiceUpdateFailed(RpcError<TransportErrorKind>),
    /// The finalized head is behind the unsafe head.
    #[error("Invalid forkchoice state: unsafe head {0} is ahead of finalized head {1}")]
    FinalizedAheadOfUnsafe(u64, u64),
    /// The forkchoice state is invalid.
    #[error("Invalid forkchoice state")]
    InvalidForkchoiceState,
    /// The payload status is unexpected.
    #[error("Unexpected payload status: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
}

impl EngineTaskError for SynchronizeTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::FinalizedAheadOfUnsafe(_, _) => EngineTaskErrorSeverity::Critical,
            // Transient RPC failure: retry the forkchoice call in place.
            Self::ForkchoiceUpdateFailed(_) => EngineTaskErrorSeverity::Temporary,
            // An INVALID forkchoice status means the engine rejected the current head (a poisoned
            // unsafe head). Retrying in place wedges derivation forever, so escalate to a reset,
            // which re-discovers a canonical head from the EL. Mirrors op-node's
            // `tryUpdateEngineInternal`, which maps a non-VALID forkchoice status to a reset.
            Self::UnexpectedPayloadStatus(_) | Self::InvalidForkchoiceState => {
                EngineTaskErrorSeverity::Reset
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_engine::PayloadStatusEnum;
    use alloy_transport::RpcError;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::finalized_ahead_of_unsafe(
        SynchronizeTaskError::FinalizedAheadOfUnsafe(10, 5),
        EngineTaskErrorSeverity::Critical
    )]
    #[case::rpc_failure_is_temporary(
        SynchronizeTaskError::ForkchoiceUpdateFailed(RpcError::local_usage_str("test")),
        EngineTaskErrorSeverity::Temporary
    )]
    #[case::invalid_forkchoice_state_resets(
        SynchronizeTaskError::InvalidForkchoiceState,
        EngineTaskErrorSeverity::Reset
    )]
    #[case::invalid_payload_status_resets(
        SynchronizeTaskError::UnexpectedPayloadStatus(PayloadStatusEnum::Invalid {
            validation_error: String::new(),
        }),
        EngineTaskErrorSeverity::Reset
    )]
    fn severity_escalates_invalid_forkchoice_to_reset(
        #[case] error: SynchronizeTaskError,
        #[case] expected: EngineTaskErrorSeverity,
    ) {
        assert_eq!(error.severity(), expected);
    }
}
