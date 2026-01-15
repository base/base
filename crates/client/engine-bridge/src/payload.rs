//! Payload-related Engine API methods.
//!
//! This module contains the `new_payload_*` and `get_payload_*` method implementations
//! for [`InProcessEngineDriver`].

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV3,
    PayloadId, PayloadStatus,
};
use base_engine_driver::DirectEngineError;
use op_alloy_rpc_types_engine::{
    OpExecutionData, OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4,
    OpExecutionPayloadV4,
};
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::debug;

use crate::InProcessEngineDriver;

impl<P> InProcessEngineDriver<P>
where
    P: BlockNumReader + HeaderProvider + BlockHashReader + Send + Sync + 'static,
{
    /// Handles `engine_newPayloadV1` - deprecated on OP Stack.
    pub(crate) async fn handle_new_payload_v1(
        &self,
        _payload: ExecutionPayloadV1,
    ) -> Result<PayloadStatus, DirectEngineError> {
        Err(DirectEngineError::Internal("new_payload_v1 is not supported on OP Stack".into()))
    }

    /// Handles `engine_newPayloadV2`.
    pub(crate) async fn handle_new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> Result<PayloadStatus, DirectEngineError> {
        debug!("Calling new_payload_v2 via channel");
        let execution_data = OpExecutionData::v2(payload);
        self.engine_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| DirectEngineError::Internal(e.to_string()))
    }

    /// Handles `engine_newPayloadV3`.
    pub(crate) async fn handle_new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, DirectEngineError> {
        debug!(
            block_hash = ?payload.payload_inner.payload_inner.block_hash,
            "Calling new_payload_v3 via channel"
        );
        let execution_data =
            OpExecutionData::v3(payload, versioned_hashes, parent_beacon_block_root);
        self.engine_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| DirectEngineError::Internal(e.to_string()))
    }

    /// Handles `engine_newPayloadV4`.
    pub(crate) async fn handle_new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> Result<PayloadStatus, DirectEngineError> {
        debug!(
            block_hash = ?payload.payload_inner.payload_inner.payload_inner.block_hash,
            "Calling new_payload_v4 via channel"
        );
        let execution_data = OpExecutionData::v4(
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        );
        self.engine_handle
            .new_payload(execution_data)
            .await
            .map_err(|e| DirectEngineError::Internal(e.to_string()))
    }

    /// Handles `engine_getPayloadV2`.
    pub(crate) async fn handle_get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> Result<ExecutionPayloadEnvelopeV2, DirectEngineError> {
        debug!(%payload_id, "Calling get_payload_v2 via channel");
        Ok(self
            .payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| DirectEngineError::Internal("payload not found".into()))?
            .map_err(|e| DirectEngineError::Internal(e.to_string()))?
            .into())
    }

    /// Handles `engine_getPayloadV3`.
    pub(crate) async fn handle_get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV3, DirectEngineError> {
        debug!(%payload_id, "Calling get_payload_v3 via channel");
        Ok(self
            .payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| DirectEngineError::Internal("payload not found".into()))?
            .map_err(|e| DirectEngineError::Internal(e.to_string()))?
            .into())
    }

    /// Handles `engine_getPayloadV4`.
    pub(crate) async fn handle_get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV4, DirectEngineError> {
        debug!(%payload_id, "Calling get_payload_v4 via channel");
        Ok(self
            .payload_store
            .resolve(payload_id)
            .await
            .ok_or_else(|| DirectEngineError::Internal("payload not found".into()))?
            .map_err(|e| DirectEngineError::Internal(e.to_string()))?
            .into())
    }
}
