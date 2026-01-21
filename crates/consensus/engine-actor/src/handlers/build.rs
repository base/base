//! Build request handler.

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadId, PayloadStatusEnum};
use base_engine_ext::InProcessEngineClient;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::{debug, warn};

use crate::{EngineSyncState, ProcessorError};

/// Handler for build requests.
#[derive(Debug)]
pub struct BuildHandler;

impl BuildHandler {
    /// Handles a build request by calling `fork_choice_updated_v3` with attributes.
    ///
    /// This initiates payload building in the execution layer.
    pub async fn handle<P>(
        client: &InProcessEngineClient<P>,
        sync_state: &EngineSyncState,
        parent_hash: B256,
        attributes: OpPayloadAttributes,
    ) -> Result<PayloadId, ProcessorError>
    where
        P: BlockNumReader + BlockHashReader + HeaderProvider,
    {
        debug!(?parent_hash, "Handling build request");

        // Build forkchoice state from current sync state.
        let safe_hash = sync_state.safe_head_hash().unwrap_or(parent_hash);
        let finalized_hash = sync_state.finalized_head_hash().unwrap_or(parent_hash);

        let state = ForkchoiceState {
            head_block_hash: parent_hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_hash,
        };

        let response = client
            .fork_choice_updated_v3(state, Some(attributes))
            .await
            .map_err(ProcessorError::Engine)?;

        // Validate payload status.
        match response.payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                warn!("FCU returned ACCEPTED status, expected VALID or SYNCING");
            }
        }

        response.payload_id.ok_or(ProcessorError::MissingPayloadId)
    }
}
