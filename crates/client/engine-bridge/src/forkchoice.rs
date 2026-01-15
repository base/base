//! Fork choice Engine API methods.
//!
//! This module contains the `fork_choice_updated_*` method implementations
//! for [`InProcessEngineDriver`].

use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use base_engine_driver::DirectEngineError;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::debug;

use crate::InProcessEngineDriver;

impl<P> InProcessEngineDriver<P>
where
    P: BlockNumReader + HeaderProvider + BlockHashReader + Send + Sync + 'static,
{
    /// Handles `engine_forkchoiceUpdatedV2`.
    pub(crate) async fn handle_fork_choice_updated_v2(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, DirectEngineError> {
        debug!(head = ?state.head_block_hash, "Calling fork_choice_updated_v2 via channel");
        self.engine_handle
            .fork_choice_updated(state, payload_attributes, EngineApiMessageVersion::V2)
            .await
            .map_err(|e| DirectEngineError::Internal(e.to_string()))
    }

    /// Handles `engine_forkchoiceUpdatedV3`.
    pub(crate) async fn handle_fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, DirectEngineError> {
        debug!(head = ?state.head_block_hash, "Calling fork_choice_updated_v3 via channel");
        self.engine_handle
            .fork_choice_updated(state, payload_attributes, EngineApiMessageVersion::V3)
            .await
            .map_err(|e| DirectEngineError::Internal(e.to_string()))
    }
}
