//! Fork choice update methods.

use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_payload_primitives::EngineApiMessageVersion;
use tracing::debug;

use crate::{EngineError, InProcessEngineClient};

impl InProcessEngineClient {
    /// Updates the fork choice state (Engine API v2).
    pub async fn fork_choice_updated_v2(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineError> {
        debug!(head = ?state.head_block_hash, "Sending fork_choice_updated_v2 via channel");
        self.consensus_handle
            .fork_choice_updated(state, payload_attributes, EngineApiMessageVersion::V2)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }

    /// Updates the fork choice state (Engine API v3).
    pub async fn fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineError> {
        debug!(head = ?state.head_block_hash, "Sending fork_choice_updated_v3 via channel");
        self.consensus_handle
            .fork_choice_updated(state, payload_attributes, EngineApiMessageVersion::V3)
            .await
            .map_err(|e| EngineError::Engine(e.to_string()))
    }
}
