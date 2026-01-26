//! Payload-related engine methods.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2, ExecutionPayloadV3, PayloadId,
    PayloadStatus,
};
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadV4,
};
use tracing::debug;

use crate::{EngineError, InProcessEngineClient};

impl<P> InProcessEngineClient<P> {
    /// Sends a new payload to the execution layer (Engine API v2).
    pub async fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> Result<PayloadStatus, EngineError> {
        debug!("Sending new_payload_v2 via channel");
        let execution_data = OpExecutionData::v2(payload);
        self.consensus_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }

    /// Sends a new payload to the execution layer (Engine API v3).
    pub async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, EngineError> {
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

    /// Sends a new payload to the execution layer (Engine API v4).
    pub async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> Result<PayloadStatus, EngineError> {
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

    /// Retrieves a payload by ID (Engine API v2).
    pub async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> Result<ExecutionPayloadEnvelopeV2, EngineError> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineError::PayloadNotFound(payload_id.to_string()))?
            .map(Into::into)
            .map_err(|e| EngineError::PayloadResolution(e.to_string()))
    }

    /// Retrieves a payload by ID (Engine API v3).
    pub async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV3, EngineError> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineError::PayloadNotFound(payload_id.to_string()))?
            .map(Into::into)
            .map_err(|e| EngineError::PayloadResolution(e.to_string()))
    }

    /// Retrieves a payload by ID (Engine API v4).
    pub async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV4, EngineError> {
        debug!(%payload_id, "Resolving payload via PayloadStore");
        self.payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| EngineError::PayloadNotFound(payload_id.to_string()))?
            .map(Into::into)
            .map_err(|e| EngineError::PayloadResolution(e.to_string()))
    }
}
