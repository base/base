//! Finalization request handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use base_engine_ext::DirectEngineApi;
use tracing::{debug, warn};

use crate::{EngineSyncState, ProcessorError};

/// Handler for finalization (ProcessFinalizedL2BlockNumber) requests.
#[derive(Debug)]
pub struct FinalizationHandler;

impl FinalizationHandler {
    /// Handles a finalized L2 block number by updating the finalized head.
    pub async fn handle<E>(
        client: &E,
        sync_state: &EngineSyncState,
        finalized_number: u64,
    ) -> Result<(), ProcessorError>
    where
        E: DirectEngineApi,
    {
        debug!(finalized_number, "Handling finalization request");

        // Get the block info for the finalized number.
        let finalized_info = client
            .l2_block_info_by_number(finalized_number)
            .await
            .map_err(ProcessorError::engine)?
            .ok_or_else(|| {
                ProcessorError::BlockNotFound(format!("block number {finalized_number}"))
            })?;

        // Get current heads.
        let unsafe_hash = sync_state.unsafe_head_hash().unwrap_or(finalized_info.block_info.hash);
        let safe_hash = sync_state.safe_head_hash().unwrap_or(finalized_info.block_info.hash);

        // Update fork choice with new finalized head.
        let state = ForkchoiceState {
            head_block_hash: unsafe_hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_info.block_info.hash,
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
                warn!("FCU for finalization returned ACCEPTED status");
            }
        }

        // Update sync state.
        sync_state.set_finalized_head(finalized_info);

        Ok(())
    }
}
