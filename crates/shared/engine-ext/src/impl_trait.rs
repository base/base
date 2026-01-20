//! DirectEngineApi implementation for InProcessEngineClient.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV3,
    ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use async_trait::async_trait;
use kona_protocol::L2BlockInfo;
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadV4, OpPayloadAttributes,
};
use reth_payload_primitives::EngineApiMessageVersion;
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::debug;

use crate::{DirectEngineApi, EngineError, InProcessEngineClient};

#[async_trait]
impl<P> DirectEngineApi for InProcessEngineClient<P>
where
    P: BlockNumReader + BlockHashReader + HeaderProvider + Send + Sync + 'static,
{
    type Error = EngineError;

    // ========================================================================
    // New Payload Methods
    // ========================================================================

    async fn new_payload_v1(
        &self,
        payload: ExecutionPayloadV1,
    ) -> Result<PayloadStatus, Self::Error> {
        debug!("Sending new_payload_v1 via channel");
        // V1 payloads don't have withdrawals - wrap in V2 input format.
        let input_v2 = ExecutionPayloadInputV2 { execution_payload: payload, withdrawals: None };
        let execution_data = OpExecutionData::v2(input_v2);
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }

    async fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> Result<PayloadStatus, Self::Error> {
        debug!("Sending new_payload_v2 via channel");
        let execution_data = OpExecutionData::v2(payload);
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, Self::Error> {
        debug!(
            block_hash = ?payload.payload_inner.payload_inner.block_hash,
            "Sending new_payload_v3 via channel"
        );
        let execution_data =
            OpExecutionData::v3(payload, versioned_hashes, parent_beacon_block_root);
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }

    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> Result<PayloadStatus, Self::Error> {
        debug!(
            block_hash = ?payload.payload_inner.payload_inner.payload_inner.block_hash,
            "Sending new_payload_v4 via channel"
        );
        let execution_data = OpExecutionData::v4(
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        );
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }

    // ========================================================================
    // Fork Choice Methods
    // ========================================================================

    async fn fork_choice_updated_v2(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, Self::Error> {
        debug!(head = ?state.head_block_hash, "Sending fork_choice_updated_v2 via channel");
        let result = self
            .consensus_handle
            .fork_choice_updated(state, payload_attributes, EngineApiMessageVersion::V2)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))?;

        // Update the forkchoice tracker with the new state.
        self.forkchoice.update(state);

        Ok(result)
    }

    async fn fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, Self::Error> {
        debug!(head = ?state.head_block_hash, "Sending fork_choice_updated_v3 via channel");
        let result = self
            .consensus_handle
            .fork_choice_updated(state, payload_attributes, EngineApiMessageVersion::V3)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))?;

        // Update the forkchoice tracker with the new state.
        self.forkchoice.update(state);

        Ok(result)
    }

    // ========================================================================
    // Get Payload Methods
    // ========================================================================

    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> Result<ExecutionPayloadEnvelopeV2, Self::Error> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineError::PayloadNotFound(payload_id.to_string()))?
            .map(Into::into)
            .map_err(|e| EngineError::PayloadResolution(e.to_string()))
    }

    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV3, Self::Error> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineError::PayloadNotFound(payload_id.to_string()))?
            .map(Into::into)
            .map_err(|e| EngineError::PayloadResolution(e.to_string()))
    }

    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV4, Self::Error> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineError::PayloadNotFound(payload_id.to_string()))?
            .map(Into::into)
            .map_err(|e| EngineError::PayloadResolution(e.to_string()))
    }

    // ========================================================================
    // Block Query Methods
    // ========================================================================

    async fn l2_block_info_by_number(
        &self,
        number: u64,
    ) -> Result<Option<L2BlockInfo>, Self::Error> {
        // Delegate to the synchronous inherent method.
        match self.l2_block_info_by_number_sync(number) {
            Ok(info) => Ok(Some(info)),
            Err(EngineError::BlockNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn l2_block_info_latest(&self) -> Result<Option<L2BlockInfo>, Self::Error> {
        // Delegate to the synchronous inherent method.
        match self.l2_block_info_latest_sync() {
            Ok(info) => Ok(Some(info)),
            Err(EngineError::BlockNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn l2_block_info_by_hash(&self, hash: B256) -> Result<Option<L2BlockInfo>, Self::Error> {
        // Delegate to the synchronous inherent method.
        match self.l2_block_info_by_hash_sync(hash) {
            Ok(info) => Ok(Some(info)),
            Err(EngineError::BlockNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn l2_safe_head(&self) -> Result<Option<L2BlockInfo>, Self::Error> {
        // Delegate to the synchronous inherent method.
        match self.l2_safe_head_sync() {
            Ok(info) => Ok(Some(info)),
            Err(EngineError::ForkchoiceNotInitialized) | Err(EngineError::BlockNotFound(_)) => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    async fn l2_finalized_head(&self) -> Result<Option<L2BlockInfo>, Self::Error> {
        // Delegate to the synchronous inherent method.
        match self.l2_finalized_head_sync() {
            Ok(info) => Ok(Some(info)),
            Err(EngineError::ForkchoiceNotInitialized) | Err(EngineError::BlockNotFound(_)) => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // This will fail to compile if InProcessEngineClient is not Send + Sync.
        assert_send_sync::<InProcessEngineClient<reth_provider::noop::NoopProvider>>();
    }
}
