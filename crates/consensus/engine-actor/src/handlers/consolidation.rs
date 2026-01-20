//! Consolidation (safe L2 signal) request handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use base_engine_ext::DirectEngineApi;
use kona_protocol::L2BlockInfo;
use tracing::{debug, warn};

use crate::{EngineSyncState, ProcessorError};

/// Handler for consolidation (ProcessSafeL2Signal) requests.
#[derive(Debug)]
pub struct ConsolidationHandler;

impl ConsolidationHandler {
    /// Handles a safe L2 signal by updating the safe head in the fork choice state.
    pub async fn handle<E>(
        client: &E,
        sync_state: &EngineSyncState,
        safe_head: L2BlockInfo,
    ) -> Result<(), ProcessorError>
    where
        E: DirectEngineApi,
    {
        debug!(
            safe_hash = ?safe_head.block_info.hash,
            safe_number = safe_head.block_info.number,
            "Handling consolidation request"
        );

        // Get current heads.
        let unsafe_hash = sync_state.unsafe_head_hash().unwrap_or(safe_head.block_info.hash);
        let finalized_hash = sync_state.finalized_head_hash().unwrap_or(safe_head.block_info.hash);

        // Update fork choice with new safe head.
        let state = ForkchoiceState {
            head_block_hash: unsafe_hash,
            safe_block_hash: safe_head.block_info.hash,
            finalized_block_hash: finalized_hash,
        };

        let response =
            client.fork_choice_updated_v3(state, None).await.map_err(ProcessorError::engine)?;

        // Validate response.
        match response.payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                warn!("FCU for consolidation returned ACCEPTED status");
            }
        }

        // Update sync state.
        sync_state.set_safe_head(safe_head);

        Ok(())
    }
}
