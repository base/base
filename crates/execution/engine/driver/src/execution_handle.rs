use base_common_types_payload::{
    BeaconForkChoiceUpdateError, ConsensusEngineHandle, ForkchoiceState,
    InvalidPayloadAttributesError, PayloadBuilderError, PayloadId, PayloadKind,
};
use base_execution_payload::{
    BaseBuiltPayload, BaseEngineValidator, BasePayloadBuilderAttributes, PayloadBuilderHandle,
};

/// In-process access to the execution driver and its payload builder.
#[derive(Clone, Debug)]
pub struct BaseExecutionHandle {
    /// The serialized execution command queue.
    pub driver: ConsensusEngineHandle,
    /// The service that owns active payload builds.
    pub payload_builder: PayloadBuilderHandle,
    /// Validation against the node's shared runtime upgrade schedule.
    pub validator: BaseEngineValidator,
}

/// Failures submitting native execution operations.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionCommandError {
    /// The requested forkchoice could not be applied.
    #[error(transparent)]
    Forkchoice(#[from] BeaconForkChoiceUpdateError),
    /// Build attributes violate the chain rules.
    #[error(transparent)]
    InvalidAttributes(#[from] InvalidPayloadAttributesError),
    /// The requested build does not exist or its service stopped.
    #[error("unknown payload build {0}")]
    UnknownBuild(PayloadId),
    /// Building the payload failed.
    #[error(transparent)]
    Build(#[from] PayloadBuilderError),
}

impl BaseExecutionHandle {
    /// Applies a build parent and starts a validated payload job.
    pub async fn start_building(
        &self,
        heads: ForkchoiceState,
        attributes: BasePayloadBuilderAttributes,
    ) -> Result<PayloadId, ExecutionCommandError> {
        if let Err(error) = self.validator.validate_attributes(&attributes) {
            let status = self.driver.update_heads(heads).await?;
            if matches!(status, base_common_types_payload::HeadUpdateOutcome::Syncing) {
                return Err(BeaconForkChoiceUpdateError::Syncing.into());
            }
            return Err(error.into());
        }
        Ok(self.driver.start_building(heads, attributes).await?)
    }

    /// Resolves a build to its native block, receipts, and execution metadata.
    pub async fn end_building(
        &self,
        id: PayloadId,
    ) -> Result<BaseBuiltPayload, ExecutionCommandError> {
        self.payload_builder
            .resolve_kind(id, PayloadKind::Earliest)
            .await
            .ok_or(ExecutionCommandError::UnknownBuild(id))?
            .map_err(ExecutionCommandError::Build)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use base_common_chain_config::BaseChainSpec;
    use base_common_types_payload::{
        ExecutionCommand, PayloadStatus, PayloadStatusEnum, PendingHeadUpdate,
    };
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn invalid_attributes_still_apply_forkchoice() {
        for status in [PayloadStatusEnum::Valid, PayloadStatusEnum::Syncing] {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let handle = BaseExecutionHandle {
                driver: ConsensusEngineHandle::new(sender),
                payload_builder: PayloadBuilderHandle::noop(),
                validator: BaseEngineValidator::new(Arc::new(BaseChainSpec::sepolia())),
            };
            let state = ForkchoiceState::same_hash(B256::repeat_byte(1));
            let update = handle.start_building(state, BasePayloadBuilderAttributes::default());
            let driver = async {
                let ExecutionCommand::UpdateHeads { heads: received, tx } =
                    receiver.recv().await.unwrap()
                else {
                    panic!("expected forkchoice command");
                };
                assert_eq!(received, state);
                let response = PendingHeadUpdate::valid(PayloadStatus::from_status(status.clone()));
                tx.send(Ok(response)).unwrap();
            };
            let (result, ()) = tokio::join!(update, driver);
            if status == PayloadStatusEnum::Syncing {
                assert!(matches!(
                    result,
                    Err(ExecutionCommandError::Forkchoice(BeaconForkChoiceUpdateError::Syncing))
                ));
            } else {
                assert!(matches!(result, Err(ExecutionCommandError::InvalidAttributes(_))));
            }
        }
    }
}
