//! Contains error types for the [`crate::SynchronizeTask`].

use alloy_transport::{RpcError, TransportErrorKind};
use base_alloy_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_protocol::FromBlockError;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    EngineTaskError, InsertTaskError, SynchronizeTaskError,
    task_queue::tasks::task::EngineTaskErrorSeverity,
};

/// An error that occurs when running the [`crate::SealTask`].
#[derive(Debug, Error)]
pub enum SealTaskError {
    /// Impossible to insert the payload into the engine.
    #[error(transparent)]
    PayloadInsertionFailed(#[from] Box<InsertTaskError>),
    /// The get payload call to the engine api failed.
    #[error(transparent)]
    GetPayloadFailed(RpcError<TransportErrorKind>),
    /// A deposit-only payload failed to import.
    #[error("Deposit-only payload failed to import")]
    DepositOnlyPayloadFailed,
    /// Failed to re-attempt payload import with deposit-only payload.
    #[error("Failed to re-attempt payload import with deposit-only payload")]
    DepositOnlyPayloadReattemptFailed,
    /// The payload is invalid, and the derivation pipeline must
    /// be flushed post-holocene.
    #[error("Invalid payload, must flush post-holocene")]
    HoloceneInvalidFlush,
    /// Failed to convert a [`BaseExecutionPayload`] to a [`L2BlockInfo`].
    ///
    /// [`BaseExecutionPayload`]: base_alloy_rpc_types_engine::BaseExecutionPayload
    /// [`L2BlockInfo`]: base_protocol::L2BlockInfo
    #[error(transparent)]
    FromBlock(#[from] FromBlockError),
    /// Error sending the built payload envelope.
    #[error(transparent)]
    MpscSend(#[from] Box<mpsc::error::SendError<Result<BaseExecutionPayloadEnvelope, Self>>>),
    /// The clock went backwards.
    #[error("The clock went backwards")]
    ClockWentBackwards,
    /// Unsafe head changed between build and seal. This likely means that there was some race
    /// condition between the previous seal updating the unsafe head and the build attributes
    /// being created. This build has been invalidated.
    ///
    /// If not propagated to the original caller for handling (i.e. there was no original caller),
    /// this should not happen and is a critical error.
    #[error("Unsafe head changed between build and seal")]
    UnsafeHeadChangedSinceBuild,
}

impl SealTaskError {
    /// Whether this error is fatal from the sequencer's perspective.
    ///
    /// This classification is intentionally separate from [`EngineTaskError::severity`] because
    /// the sequencer may interpret error severity differently than the engine. The exhaustive
    /// match ensures new variants cause a compile error until explicitly classified here.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::PayloadInsertionFailed(insert_err) => match &**insert_err {
                InsertTaskError::ForkchoiceUpdateFailed(synchronize_error) => {
                    match synchronize_error {
                        SynchronizeTaskError::FinalizedAheadOfUnsafe(_, _) => true,
                        SynchronizeTaskError::ForkchoiceUpdateFailed(_)
                        | SynchronizeTaskError::InvalidForkchoiceState
                        | SynchronizeTaskError::UnexpectedPayloadStatus(_) => false,
                    }
                }
                InsertTaskError::FromBlockError(_)
                | InsertTaskError::L2BlockInfoConstruction(_) => true,
                InsertTaskError::InsertFailed(_) | InsertTaskError::UnexpectedPayloadStatus(_) => {
                    false
                }
            },
            Self::GetPayloadFailed(_)
            | Self::HoloceneInvalidFlush
            | Self::UnsafeHeadChangedSinceBuild => false,
            Self::DepositOnlyPayloadFailed
            | Self::DepositOnlyPayloadReattemptFailed
            | Self::FromBlock(_)
            | Self::MpscSend(_)
            | Self::ClockWentBackwards => true,
        }
    }
}

impl EngineTaskError for SealTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::PayloadInsertionFailed(inner) => inner.severity(),
            Self::GetPayloadFailed(_) => EngineTaskErrorSeverity::Temporary,
            Self::HoloceneInvalidFlush => EngineTaskErrorSeverity::Flush,
            Self::UnsafeHeadChangedSinceBuild => EngineTaskErrorSeverity::Reset,
            Self::DepositOnlyPayloadReattemptFailed
            | Self::DepositOnlyPayloadFailed
            | Self::FromBlock(_)
            | Self::MpscSend(_)
            | Self::ClockWentBackwards => EngineTaskErrorSeverity::Critical,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_rpc_types_engine::PayloadStatusEnum;
    use alloy_transport::RpcError;
    use base_protocol::FromBlockError;
    use rstest::rstest;

    use super::*;

    fn rpc_error() -> RpcError<TransportErrorKind> {
        RpcError::local_usage_str("test")
    }

    #[rstest]
    #[case::get_payload_failed(SealTaskError::GetPayloadFailed(rpc_error()), false)]
    #[case::holocene_invalid_flush(SealTaskError::HoloceneInvalidFlush, false)]
    #[case::unsafe_head_changed(SealTaskError::UnsafeHeadChangedSinceBuild, false)]
    #[case::deposit_only_failed(SealTaskError::DepositOnlyPayloadFailed, true)]
    #[case::deposit_only_reattempt_failed(SealTaskError::DepositOnlyPayloadReattemptFailed, true)]
    #[case::from_block(SealTaskError::FromBlock(FromBlockError::InvalidGenesisHash), true)]
    #[case::clock_went_backwards(SealTaskError::ClockWentBackwards, true)]
    fn test_seal_task_error_is_fatal(#[case] err: SealTaskError, #[case] expected: bool) {
        assert_eq!(err.is_fatal(), expected);
    }

    #[rstest]
    #[case::finalized_ahead_of_unsafe(SynchronizeTaskError::FinalizedAheadOfUnsafe(10, 5), true)]
    #[case::forkchoice_update_failed(
        SynchronizeTaskError::ForkchoiceUpdateFailed(rpc_error()),
        false
    )]
    #[case::invalid_forkchoice_state(SynchronizeTaskError::InvalidForkchoiceState, false)]
    #[case::unexpected_payload_status(
        SynchronizeTaskError::UnexpectedPayloadStatus(PayloadStatusEnum::Invalid {
            validation_error: String::new(),
        }),
        false
    )]
    fn test_insertion_forkchoice_error_is_fatal(
        #[case] sync_err: SynchronizeTaskError,
        #[case] expected: bool,
    ) {
        let err = SealTaskError::PayloadInsertionFailed(Box::new(
            InsertTaskError::ForkchoiceUpdateFailed(sync_err),
        ));
        assert_eq!(err.is_fatal(), expected);
    }

    #[rstest]
    #[case::insert_failed(InsertTaskError::InsertFailed(rpc_error()), false)]
    #[case::unexpected_status(
        InsertTaskError::UnexpectedPayloadStatus(PayloadStatusEnum::Invalid {
            validation_error: String::new(),
        }),
        false
    )]
    #[case::l2_block_info_construction(
        InsertTaskError::L2BlockInfoConstruction(FromBlockError::InvalidGenesisHash),
        true
    )]
    fn test_insertion_non_forkchoice_error_is_fatal(
        #[case] insert_err: InsertTaskError,
        #[case] expected: bool,
    ) {
        let err = SealTaskError::PayloadInsertionFailed(Box::new(insert_err));
        assert_eq!(err.is_fatal(), expected);
    }
}
