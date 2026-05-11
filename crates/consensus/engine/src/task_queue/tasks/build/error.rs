//! Contains error types for the [`crate::SynchronizeTask`].

use alloy_rpc_types_engine::{PayloadId, PayloadStatusEnum};
use alloy_transport::{RpcError, TransportErrorKind};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{EngineTaskError, task_queue::tasks::task::EngineTaskErrorSeverity};

/// An error that occurs during payload building within the engine.
///
/// This error type is specific to the block building process and represents failures
/// that can occur during the automatic forkchoice update phase of [`BuildTask`].
/// Unlike [`BuildTaskError`], which handles higher-level build orchestration errors,
/// `EngineBuildError` focuses on low-level engine API communication failures.
///
/// ## Error Categories
///
/// - **State Validation**: Errors related to inconsistent chain state
/// - **Engine Communication**: RPC failures during forkchoice updates
/// - **Payload Validation**: Invalid payload status responses from the execution layer
///
/// [`BuildTask`]: crate::BuildTask
#[derive(Debug, Error)]
pub enum EngineBuildError {
    /// The finalized head is ahead of the unsafe head.
    #[error("Finalized head is ahead of unsafe head")]
    FinalizedAheadOfUnsafe(u64, u64),
    /// The forkchoice update call to the engine api failed.
    #[error("Failed to build payload attributes in the engine. Forkchoice RPC error: {0}")]
    AttributesInsertionFailed(#[from] RpcError<TransportErrorKind>),
    /// The engine returned an invalid forkchoice state error.
    #[error("Invalid forkchoice state")]
    ForkchoiceStateInvalid,
    /// The inserted payload is invalid.
    #[error("The inserted payload is invalid: {0}")]
    InvalidPayload(String),
    /// The inserted payload status is unexpected.
    #[error("The inserted payload status is unexpected: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
    /// The payload ID is missing.
    #[error("The inserted payload ID is missing")]
    MissingPayloadId,
    /// The engine is syncing.
    #[error("The engine is syncing")]
    EngineSyncing,
}

/// An error that occurs when running the [`crate::BuildTask`].
#[derive(Debug, Error)]
pub enum BuildTaskError {
    /// An error occurred when building the payload attributes in the engine.
    #[error("An error occurred when building the payload attributes to the engine.")]
    EngineBuildError(EngineBuildError),
    /// Error sending the built payload envelope.
    #[error(transparent)]
    MpscSend(#[from] Box<mpsc::error::SendError<PayloadId>>),
}

impl EngineTaskError for BuildTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::EngineBuildError(EngineBuildError::FinalizedAheadOfUnsafe(_, _)) => {
                EngineTaskErrorSeverity::Critical
            }
            // The execution layer rejected the payload attributes derived from the current
            // batch (e.g. malformed transaction bytes, invalid state transition).
            //
            // Per the derivation spec, the attributes must be dropped and the forkchoice
            // state must be left unchanged. Surfacing this as `Flush` causes the engine
            // processor to signal `FlushChannel` to the derivation pipeline (clearing the
            // poisoned batch + upstream channel state) while `Engine::drain` removes this
            // task from the head of the queue. Without this, the build task is retried
            // in-place forever, starving every later engine request behind it.
            Self::EngineBuildError(EngineBuildError::InvalidPayload(_)) => {
                EngineTaskErrorSeverity::Flush
            }
            Self::EngineBuildError(EngineBuildError::AttributesInsertionFailed(_))
            | Self::EngineBuildError(EngineBuildError::UnexpectedPayloadStatus(_))
            | Self::EngineBuildError(EngineBuildError::MissingPayloadId)
            | Self::EngineBuildError(EngineBuildError::EngineSyncing) => {
                EngineTaskErrorSeverity::Temporary
            }
            Self::EngineBuildError(EngineBuildError::ForkchoiceStateInvalid) => {
                EngineTaskErrorSeverity::Reset
            }
            Self::MpscSend(_) => EngineTaskErrorSeverity::Critical,
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn rpc_error() -> RpcError<TransportErrorKind> {
        RpcError::local_usage_str("test")
    }

    #[rstest]
    #[case::finalized_ahead_of_unsafe(
        BuildTaskError::EngineBuildError(EngineBuildError::FinalizedAheadOfUnsafe(10, 5)),
        EngineTaskErrorSeverity::Critical
    )]
    #[case::attributes_insertion_failed(
        BuildTaskError::EngineBuildError(EngineBuildError::AttributesInsertionFailed(rpc_error())),
        EngineTaskErrorSeverity::Temporary
    )]
    #[case::forkchoice_state_invalid(
        BuildTaskError::EngineBuildError(EngineBuildError::ForkchoiceStateInvalid),
        EngineTaskErrorSeverity::Reset
    )]
    #[case::invalid_payload(
        BuildTaskError::EngineBuildError(EngineBuildError::InvalidPayload("bad tx".into())),
        EngineTaskErrorSeverity::Flush
    )]
    #[case::unexpected_payload_status(
        BuildTaskError::EngineBuildError(EngineBuildError::UnexpectedPayloadStatus(
            PayloadStatusEnum::Syncing,
        )),
        EngineTaskErrorSeverity::Temporary
    )]
    #[case::missing_payload_id(
        BuildTaskError::EngineBuildError(EngineBuildError::MissingPayloadId),
        EngineTaskErrorSeverity::Temporary
    )]
    #[case::engine_syncing(
        BuildTaskError::EngineBuildError(EngineBuildError::EngineSyncing),
        EngineTaskErrorSeverity::Temporary
    )]
    #[case::mpsc_send(
        BuildTaskError::MpscSend(Box::new(mpsc::error::SendError(PayloadId::new([0u8; 8])))),
        EngineTaskErrorSeverity::Critical
    )]
    fn test_build_task_error_severity(
        #[case] err: BuildTaskError,
        #[case] expected: EngineTaskErrorSeverity,
    ) {
        assert_eq!(err.severity(), expected);
    }
}
