//! [`EngineApiClient`] trait implementation for [`InProcessEngineClient`].

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ClientCode, ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadEnvelopeV2,
    ExecutionPayloadInputV2, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus,
};
use base_primitives::{EngineApiClient, EngineApiError, EngineApiResult};
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadV4, OpPayloadAttributes, ProtocolVersion,
};
use reth_payload_primitives::EngineApiMessageVersion;
use tracing::debug;

use crate::InProcessEngineClient;

impl<P: Send + Sync> EngineApiClient for InProcessEngineClient<P> {
    async fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> EngineApiResult<PayloadStatus> {
        debug!("Sending new_payload_v2 via channel");
        let execution_data = OpExecutionData::v2(payload);
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineApiError::new(e.to_string()))
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        parent_beacon_block_root: B256,
    ) -> EngineApiResult<PayloadStatus> {
        debug!(
            block_hash = ?payload.payload_inner.payload_inner.block_hash,
            "Sending new_payload_v3 via channel"
        );
        // OP Stack chains always use empty versioned hashes
        let versioned_hashes: Vec<B256> = vec![];
        let execution_data =
            OpExecutionData::v3(payload, versioned_hashes, parent_beacon_block_root);
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineApiError::new(e.to_string()))
    }

    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        parent_beacon_block_root: B256,
    ) -> EngineApiResult<PayloadStatus> {
        debug!(
            block_hash = ?payload.payload_inner.payload_inner.payload_inner.block_hash,
            "Sending new_payload_v4 via channel"
        );
        // OP Stack chains always use empty versioned hashes and execution requests
        let versioned_hashes: Vec<B256> = vec![];
        let execution_requests = Default::default();
        let execution_data = OpExecutionData::v4(
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        );
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineApiError::new(e.to_string()))
    }

    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        debug!(head = ?fork_choice_state.head_block_hash, "Sending fork_choice_updated_v2 via channel");
        let result = self
            .consensus_handle
            .fork_choice_updated(fork_choice_state, payload_attributes, EngineApiMessageVersion::V2)
            .await
            .map_err(|e| EngineApiError::new(e.to_string()))?;

        // Update the forkchoice tracker with the new state.
        self.forkchoice.update(fork_choice_state);

        Ok(result)
    }

    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> EngineApiResult<ForkchoiceUpdated> {
        debug!(head = ?fork_choice_state.head_block_hash, "Sending fork_choice_updated_v3 via channel");
        let result = self
            .consensus_handle
            .fork_choice_updated(fork_choice_state, payload_attributes, EngineApiMessageVersion::V3)
            .await
            .map_err(|e| EngineApiError::new(e.to_string()))?;

        // Update the forkchoice tracker with the new state.
        self.forkchoice.update(fork_choice_state);

        Ok(result)
    }

    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<ExecutionPayloadEnvelopeV2> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineApiError::new(format!("payload not found: {payload_id}")))?
            .map(Into::into)
            .map_err(|e| EngineApiError::new(format!("payload resolution failed: {e}")))
    }

    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<OpExecutionPayloadEnvelopeV3> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineApiError::new(format!("payload not found: {payload_id}")))?
            .map(Into::into)
            .map_err(|e| EngineApiError::new(format!("payload resolution failed: {e}")))
    }

    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> EngineApiResult<OpExecutionPayloadEnvelopeV4> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineApiError::new(format!("payload not found: {payload_id}")))?
            .map(Into::into)
            .map_err(|e| EngineApiError::new(format!("payload resolution failed: {e}")))
    }

    async fn get_payload_bodies_by_hash_v1(
        &self,
        _block_hashes: Vec<B256>,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        // TODO: Implement via provider
        Err(EngineApiError::new(
            "get_payload_bodies_by_hash_v1 not yet implemented for in-process client",
        ))
    }

    async fn get_payload_bodies_by_range_v1(
        &self,
        _start: u64,
        _count: u64,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        // TODO: Implement via provider
        Err(EngineApiError::new(
            "get_payload_bodies_by_range_v1 not yet implemented for in-process client",
        ))
    }

    async fn get_client_version_v1(
        &self,
        _client_version: ClientVersionV1,
    ) -> EngineApiResult<Vec<ClientVersionV1>> {
        // Return a basic client version for in-process communication
        // We use RH (Reth) since Base is built on top of Reth
        Ok(vec![ClientVersionV1 {
            code: ClientCode::RH,
            name: "base-node".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: "in-process".to_string(),
        }])
    }

    async fn signal_superchain_v1(
        &self,
        _recommended: ProtocolVersion,
        required: ProtocolVersion,
    ) -> EngineApiResult<ProtocolVersion> {
        // For in-process communication, we just echo back the required version
        // as we're always in sync with ourselves
        Ok(required)
    }

    async fn exchange_capabilities(
        &self,
        _capabilities: Vec<String>,
    ) -> EngineApiResult<Vec<String>> {
        // Return the capabilities supported by this in-process client
        Ok(vec![
            "engine_newPayloadV2".to_string(),
            "engine_newPayloadV3".to_string(),
            "engine_newPayloadV4".to_string(),
            "engine_forkchoiceUpdatedV2".to_string(),
            "engine_forkchoiceUpdatedV3".to_string(),
            "engine_getPayloadV2".to_string(),
            "engine_getPayloadV3".to_string(),
            "engine_getPayloadV4".to_string(),
            "engine_getClientVersionV1".to_string(),
            "engine_signalSuperchainV1".to_string(),
        ])
    }
}
