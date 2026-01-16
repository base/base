//! In-process engine driver implementation.
//!
//! This module provides [`InProcessEngineDriver`] which communicates with reth's
//! Engine API directly via channels, providing zero-overhead in-process communication
//! without HTTP or IPC overhead.

use std::sync::Arc;

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV3,
    ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use async_trait::async_trait;
use base_engine_driver::{DirectEngineApi, DirectEngineError};
use kona_protocol::L2BlockInfo;
use op_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4,
    OpPayloadAttributes,
};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_optimism_node::OpEngineTypes;
use reth_payload_builder::PayloadStore;
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};

/// In-process engine driver that communicates with reth's Engine API directly via channels.
///
/// This driver uses reth's internal [`ConsensusEngineHandle`] and [`PayloadStore`] to communicate
/// with the engine, completely bypassing JSON-RPC serialization and network overhead.
///
/// # Architecture
///
/// ```text
/// ┌─────────────────┐     ┌──────────────────────┐     ┌─────────────────────────┐
/// │  Consensus      │     │ InProcessEngineDriver│     │  reth Engine            │
/// │  (kona-node)    │────▶│ (channel-based)      │────▶│  (EngineApiTreeHandler) │
/// └─────────────────┘     └──────────────────────┘     └─────────────────────────┘
///                               │                            │
///                               │    Direct channel send     │
///                               └────────────────────────────┘
/// ```
///
/// # Type Parameters
///
/// * `P` - The provider type for block queries (must implement `BlockNumReader + HeaderProvider + BlockHashReader`)
#[derive(Debug)]
pub struct InProcessEngineDriver<P> {
    /// Handle to communicate with reth's consensus engine.
    pub(crate) engine_handle: ConsensusEngineHandle<OpEngineTypes>,
    /// Store for retrieving built payloads.
    pub(crate) payload_store: PayloadStore<OpEngineTypes>,
    /// Provider for block queries.
    pub(crate) provider: Arc<P>,
}

impl<P> InProcessEngineDriver<P>
where
    P: BlockNumReader + HeaderProvider + BlockHashReader + Send + Sync + 'static,
{
    /// Creates a new `InProcessEngineDriver` with the given engine handle, payload store, and provider.
    ///
    /// # Arguments
    ///
    /// * `engine_handle` - Handle to reth's consensus engine (from `AddOnsContext::beacon_engine_handle`)
    /// * `payload_store` - Store for built payloads (from `PayloadBuilderHandle`)
    /// * `provider` - The block provider for querying chain state
    pub const fn new(
        engine_handle: ConsensusEngineHandle<OpEngineTypes>,
        payload_store: PayloadStore<OpEngineTypes>,
        provider: Arc<P>,
    ) -> Self {
        Self { engine_handle, payload_store, provider }
    }

    /// Returns a reference to the engine handle.
    pub const fn engine_handle(&self) -> &ConsensusEngineHandle<OpEngineTypes> {
        &self.engine_handle
    }

    /// Returns a reference to the payload store.
    pub const fn payload_store(&self) -> &PayloadStore<OpEngineTypes> {
        &self.payload_store
    }
}

#[async_trait]
impl<P> DirectEngineApi for InProcessEngineDriver<P>
where
    P: BlockNumReader + HeaderProvider + BlockHashReader + Send + Sync + 'static,
{
    type Error = DirectEngineError;

    // ========================================================================
    // New Payload Methods
    // ========================================================================

    async fn new_payload_v1(
        &self,
        payload: ExecutionPayloadV1,
    ) -> Result<PayloadStatus, Self::Error> {
        self.handle_new_payload_v1(payload).await
    }

    async fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> Result<PayloadStatus, Self::Error> {
        self.handle_new_payload_v2(payload).await
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, Self::Error> {
        self.handle_new_payload_v3(payload, versioned_hashes, parent_beacon_block_root).await
    }

    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> Result<PayloadStatus, Self::Error> {
        self.handle_new_payload_v4(
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        )
        .await
    }

    // ========================================================================
    // Fork Choice Methods
    // ========================================================================

    async fn fork_choice_updated_v2(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, Self::Error> {
        self.handle_fork_choice_updated_v2(state, payload_attributes).await
    }

    async fn fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, Self::Error> {
        self.handle_fork_choice_updated_v3(state, payload_attributes).await
    }

    // ========================================================================
    // Get Payload Methods
    // ========================================================================

    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> Result<ExecutionPayloadEnvelopeV2, Self::Error> {
        self.handle_get_payload_v2(payload_id).await
    }

    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV3, Self::Error> {
        self.handle_get_payload_v3(payload_id).await
    }

    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV4, Self::Error> {
        self.handle_get_payload_v4(payload_id).await
    }

    // ========================================================================
    // Block Query Methods
    // ========================================================================

    async fn l2_block_info_by_number(
        &self,
        number: u64,
    ) -> Result<Option<L2BlockInfo>, Self::Error> {
        self.handle_l2_block_info_by_number(number).await
    }

    async fn l2_block_info_latest(&self) -> Result<Option<L2BlockInfo>, Self::Error> {
        self.handle_l2_block_info_latest().await
    }

    async fn l2_block_info_by_hash(&self, hash: B256) -> Result<Option<L2BlockInfo>, Self::Error> {
        self.handle_l2_block_info_by_hash(hash).await
    }

    async fn l2_safe_head(&self) -> Result<Option<L2BlockInfo>, Self::Error> {
        self.handle_l2_safe_head().await
    }

    async fn l2_finalized_head(&self) -> Result<Option<L2BlockInfo>, Self::Error> {
        self.handle_l2_finalized_head().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // This will fail to compile if InProcessEngineDriver is not Send + Sync
        assert_send_sync::<InProcessEngineDriver<reth_provider::noop::NoopProvider>>();
    }
}
