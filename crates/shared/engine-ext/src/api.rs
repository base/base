//! [`EngineApiClient`] trait implementation for [`InProcessEngineClient`].

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ClientCode, ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadEnvelopeV2,
    ExecutionPayloadInputV2, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus,
};
use base_primitives::{EngineApiClient, EngineApiError, EngineApiResult};
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadV4, OpPayloadAttributes,
};
use reth_optimism_primitives::OpBlock;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_primitives_traits::Block;
use reth_storage_api::BlockReader;
use tracing::debug;

use crate::InProcessEngineClient;

impl<P: Send + Sync + BlockReader<Block = OpBlock>> EngineApiClient for InProcessEngineClient<P> {
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
            .fork_choice_updated(
                fork_choice_state,
                payload_attributes,
                EngineApiMessageVersion::V2,
            )
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
            .fork_choice_updated(
                fork_choice_state,
                payload_attributes,
                EngineApiMessageVersion::V3,
            )
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
        block_hashes: Vec<B256>,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        debug!(count = block_hashes.len(), "Fetching payload bodies by hash");

        let mut bodies = Vec::with_capacity(block_hashes.len());

        for hash in block_hashes {
            // Use the provider to look up the block body by hash
            let body = self
                .provider
                .block_by_hash(hash)
                .map_err(|e| EngineApiError::new(format!("provider error: {e}")))?
                .map(|block| convert_block_to_payload_body(&block));
            bodies.push(body);
        }

        Ok(bodies)
    }

    async fn get_payload_bodies_by_range_v1(
        &self,
        start: u64,
        count: u64,
    ) -> EngineApiResult<ExecutionPayloadBodiesV1> {
        debug!(start, count, "Fetching payload bodies by range");

        if count == 0 {
            return Ok(vec![]);
        }

        let mut bodies = Vec::with_capacity(count as usize);

        for number in start..start.saturating_add(count) {
            let body = self
                .provider
                .block_by_number(number)
                .map_err(|e| EngineApiError::new(format!("provider error: {e}")))?
                .map(|block| convert_block_to_payload_body(&block));
            bodies.push(body);
        }

        Ok(bodies)
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
            "engine_getPayloadBodiesByHashV1".to_string(),
            "engine_getPayloadBodiesByRangeV1".to_string(),
        ])
    }
}

/// Converts an [`OpBlock`] to an [`ExecutionPayloadBodyV1`].
fn convert_block_to_payload_body(block: &OpBlock) -> alloy_rpc_types_engine::ExecutionPayloadBodyV1 {
    let transactions = block
        .body()
        .transactions()
        .map(|tx| {
            let mut buf = Vec::new();
            tx.encode_2718(&mut buf);
            buf.into()
        })
        .collect();
    let withdrawals = block.body().withdrawals.clone().map(|w| w.into_inner());
    alloy_rpc_types_engine::ExecutionPayloadBodyV1 { transactions, withdrawals }
}
