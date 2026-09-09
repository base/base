use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadId};
use base_common_consensus::BaseTxEnvelope;
use base_execution_payload_types::{
    InvalidPayloadAttributesError, PayloadBuilderError, PayloadKind,
};
use reth_engine_primitives::{BeaconForkChoiceUpdateError, ConsensusEngineHandle};

use crate::{
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
    /// Applies forkchoice and optionally starts a validated payload build.
    ///
    /// Invalid build attributes must not roll back a valid forkchoice update. An invalid or
    /// syncing head takes precedence over the build-attribute failure.
    pub async fn update_forkchoice(
        &self,
        state: ForkchoiceState,
        attributes: Option<BasePayloadBuilderAttributes<BaseTxEnvelope>>,
    ) -> Result<ForkchoiceUpdated, ExecutionCommandError> {
        if let Some(attributes) = &attributes {
            if let Err(error) = self.validator.validate_attributes(attributes) {
                let result = self.driver.fork_choice_updated(state, None).await?;
                if result.is_invalid() || result.payload_status.is_syncing() {
                    return Ok(result);
                }
                return Err(error.into());
            }
        }
        Ok(self.driver.fork_choice_updated(state, attributes).await?)
    }

    /// Resolves a build to its native block, receipts, and execution metadata.
    pub async fn resolve_payload(
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
    use alloy_rpc_types_engine::{PayloadStatus, PayloadStatusEnum};
    use base_execution_chainspec::BaseChainSpec;
    use reth_engine_primitives::{BeaconEngineMessage, OnForkChoiceUpdated};
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
            let update =
                handle.update_forkchoice(state, Some(BasePayloadBuilderAttributes::default()));
            let driver = async {
                let BeaconEngineMessage::ForkchoiceUpdated { state: received, payload_attrs, tx } =
                    receiver.recv().await.unwrap()
                else {
                    panic!("expected forkchoice command");
                };
                assert_eq!(received, state);
                assert!(payload_attrs.is_none());
                let response =
                    OnForkChoiceUpdated::valid(PayloadStatus::from_status(status.clone()));
                tx.send(Ok(response)).unwrap();
            };
            let (result, ()) = tokio::join!(update, driver);
            if status == PayloadStatusEnum::Syncing {
                assert!(result.unwrap().payload_status.is_syncing());
            } else {
                assert!(matches!(result, Err(ExecutionCommandError::InvalidAttributes(_))));
            }
        }
    }
}
