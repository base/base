//! Direct execution commands for integration block production.
use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use base_common_types_payload::{
    BaseExecutionPayloadEnvelopeV4, BaseExecutionPayloadV4, BasePayloadBuilderAttributes,
    ExecutionData, ForkchoiceState, PayloadId, PayloadStatus,
};
use base_execution_engine_driver::BaseExecutionHandle;
/// Integration access to execution and its payload builder.
#[derive(Clone, Debug)]
pub struct EngineApi {
    /// Native execution services owned by the test node.
    pub execution: BaseExecutionHandle,
}
impl EngineApi {
    /// Resolves a build without appending it.
    pub async fn end_building(
        &self,
        id: PayloadId,
    ) -> eyre::Result<BaseExecutionPayloadEnvelopeV4> {
        Ok(self.execution.end_building(id).await?.into())
    }
    /// Appends a fixture payload, retaining existing safety markers.
    pub async fn append_payload(
        &self,
        payload: BaseExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> eyre::Result<PayloadStatus> {
        let payload = ExecutionData::v4(
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        );
        let heads = ForkchoiceState {
            head_block_hash: payload.block_hash(),
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        };
        Ok(self
            .execution
            .driver
            .append_payload(payload, heads)
            .await
            .map(|outcome| outcome.into_payload_status())
            .or_else(|error| match error {
                base_common_types_payload::AppendPayloadError::InvalidPayload(status) => Ok(status),
                error => Err(error),
            })?)
    }
    /// Applies the requested canonical and safety heads.
    pub async fn update_heads(
        &self,
        current_head: B256,
        new_head: B256,
    ) -> eyre::Result<PayloadStatus> {
        Ok(self
            .execution
            .driver
            .update_heads(ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            })
            .await?
            .into_payload_status())
    }
    /// Starts a validated build on the selected parent.
    pub async fn start_building(
        &self,
        current_head: B256,
        parent: B256,
        attributes: BasePayloadBuilderAttributes,
    ) -> eyre::Result<PayloadId> {
        Ok(self
            .execution
            .start_building(
                ForkchoiceState {
                    head_block_hash: parent,
                    safe_block_hash: current_head,
                    finalized_block_hash: current_head,
                },
                attributes,
            )
            .await?)
    }
}
