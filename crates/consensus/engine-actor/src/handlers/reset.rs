//! Reset request handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use base_engine_ext::InProcessEngineClient;
use kona_protocol::L2BlockInfo;
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::{debug, warn};

use crate::{EngineSyncState, ProcessorError};

/// Handler for reset requests.
#[derive(Debug)]
pub struct ResetHandler;

impl ResetHandler {
    /// Handles a reset request by resetting to the finalized head.
    ///
    /// This is used when the derivation pipeline needs to be reset,
    /// typically after detecting a reorg or invalid state.
    pub async fn handle<P>(
        client: &InProcessEngineClient<P>,
        sync_state: &EngineSyncState,
    ) -> Result<L2BlockInfo, ProcessorError>
    where
        P: BlockNumReader + BlockHashReader + HeaderProvider,
    {
        debug!("Handling reset request");

        // Get the finalized head from the engine client.
        let finalized = client.l2_finalized_head().map_err(ProcessorError::Engine)?;

        // Update fork choice to the finalized head.
        let state = ForkchoiceState {
            head_block_hash: finalized.block_info.hash,
            safe_block_hash: finalized.block_info.hash,
            finalized_block_hash: finalized.block_info.hash,
        };

        let response =
            client.fork_choice_updated_v3(state, None).await.map_err(ProcessorError::Engine)?;

        // Validate response.
        match response.payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                warn!("FCU for reset returned ACCEPTED status");
            }
        }

        // Reset sync state to finalized head.
        sync_state.set_unsafe_head(finalized);
        sync_state.set_safe_head(finalized);
        sync_state.set_finalized_head(finalized);

        Ok(finalized)
    }
}
