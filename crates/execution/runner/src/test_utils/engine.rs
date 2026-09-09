//! Direct execution commands for integration block production.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelopeV4, BaseExecutionPayloadV4, ExecutionData, ForkchoiceState,
    ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use base_execution_payload_builder::BaseExecutionHandle;
use base_execution_payload_types::BasePayloadBuilderAttributes;

/// Integration access to the local execution driver and payload builder.
#[derive(Clone, Debug)]
pub struct EngineApi {
    /// Native execution services owned by the test node.
    pub execution: BaseExecutionHandle,
}

impl EngineApi {
    /// Resolves a build for the test fixture.
    pub async fn get_payload(&self, id: PayloadId) -> eyre::Result<BaseExecutionPayloadEnvelopeV4> {
        Ok(self.execution.resolve_payload(id).await?.into())
    }

    /// Submits a fixture payload to the serialized validation queue.
    pub async fn new_payload(
        &self,
        payload: BaseExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> eyre::Result<PayloadStatus> {
        Ok(self
            .execution
            .driver
            .new_payload(ExecutionData::v4(
                payload,
                versioned_hashes,
                parent_beacon_block_root,
                execution_requests,
            ))
            .await?)
    }

    /// Applies forkchoice and optionally starts a validated build.
    pub async fn update_forkchoice(
        &self,
        current_head: B256,
        new_head: B256,
        payload_attributes: Option<BasePayloadBuilderAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        Ok(self
            .execution
            .update_forkchoice(
                ForkchoiceState {
                    head_block_hash: new_head,
                    safe_block_hash: current_head,
                    finalized_block_hash: current_head,
                },
                payload_attributes,
            )
            .await?)
    }
}
