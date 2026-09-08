use alloy_rlp::Decodable;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use async_trait::async_trait;
use base_execution_payload_types::BaseBuiltPayload;
use jsonrpsee_core::RpcResult;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_primitives_traits::SealedBlock;
use reth_rpc_api::{RethEngineApiServer, RethNewPayloadInput, RethPayloadStatus};
use tracing::trace;

use crate::EngineApiError;

/// Standalone implementation of the `reth_` engine API namespace.
///
/// Provides the `reth_newPayload` endpoint that accepts either `ExecutionData` directly or an
/// RLP-encoded block, optionally waiting for persistence, execution cache, and sparse trie locks
/// before processing, and returns timing breakdowns with server-measured execution latency.
#[derive(Debug)]
pub struct RethEngineApi {
    beacon_engine_handle: ConsensusEngineHandle,
}

impl RethEngineApi {
    /// Creates a new [`RethEngineApi`].
    pub const fn new(beacon_engine_handle: ConsensusEngineHandle) -> Self {
        Self { beacon_engine_handle }
    }
}

#[async_trait]
impl RethEngineApiServer<base_common_rpc_types_engine::ExecutionData> for RethEngineApi {
    async fn reth_new_payload(
        &self,
        input: RethNewPayloadInput<base_common_rpc_types_engine::ExecutionData>,
        wait_for_persistence: Option<bool>,
        wait_for_caches: Option<bool>,
    ) -> RpcResult<RethPayloadStatus> {
        let wait_for_persistence = wait_for_persistence.unwrap_or(true);
        let wait_for_caches = wait_for_caches.unwrap_or(true);
        trace!(target: "rpc::engine", wait_for_persistence, wait_for_caches, "Serving reth_newPayload");

        let payload = match input {
            RethNewPayloadInput::ExecutionData(data) => data,
            RethNewPayloadInput::BlockRlp { block: block_rlp, bal } => {
                let block = Decodable::decode(&mut block_rlp.as_ref())
                    .map_err(|err| EngineApiError::Internal(Box::new(err)))?;
                BaseBuiltPayload::block_to_payload(SealedBlock::new_unhashed(block), bal)
            }
        };

        let (status, timings) = self
            .beacon_engine_handle
            .reth_new_payload(payload, wait_for_persistence, wait_for_caches)
            .await
            .map_err(EngineApiError::from)?;
        Ok(RethPayloadStatus {
            status,
            latency_us: timings.latency.as_micros() as u64,
            persistence_wait_us: timings.persistence_wait.as_micros() as u64,
            execution_cache_wait_us: timings.execution_cache_wait.map(|d| d.as_micros() as u64),
            sparse_trie_wait_us: timings.sparse_trie_wait.map(|d| d.as_micros() as u64),
        })
    }

    async fn reth_forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving reth_forkchoiceUpdated");
        self.beacon_engine_handle
            .fork_choice_updated(forkchoice_state, None)
            .await
            .map_err(|e| EngineApiError::from(e).into())
    }
}
