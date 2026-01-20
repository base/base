//! Seal request handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadId, PayloadStatusEnum};
use base_engine_ext::InProcessEngineClient;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelopeV3;
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::{debug, warn};

use crate::{EngineSyncState, ProcessorError};

/// Handler for seal requests.
#[derive(Debug)]
pub struct SealHandler;

impl SealHandler {
    /// Handles a seal request by getting the payload and inserting it via `new_payload`.
    ///
    /// This retrieves the built payload, inserts it into the execution layer,
    /// and updates the fork choice to make it canonical.
    pub async fn handle<P>(
        client: &InProcessEngineClient<P>,
        sync_state: &EngineSyncState,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV3, ProcessorError>
    where
        P: BlockNumReader + BlockHashReader + HeaderProvider,
    {
        debug!(%payload_id, "Handling seal request");

        // Get the payload from the store.
        let envelope = client.get_payload_v3(payload_id).await.map_err(ProcessorError::Engine)?;

        let block_hash = envelope.execution_payload.payload_inner.payload_inner.block_hash;

        // Insert the payload via new_payload.
        let status = client
            .new_payload_v3(
                envelope.execution_payload.clone(),
                vec![], // No blob versioned hashes for OP.
                envelope.parent_beacon_block_root,
            )
            .await
            .map_err(ProcessorError::Engine)?;

        // Validate payload status.
        match status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                warn!("new_payload returned ACCEPTED status, expected VALID or SYNCING");
            }
        }

        // Update fork choice to make the block canonical.
        let safe_hash = sync_state.safe_head_hash().unwrap_or(block_hash);
        let finalized_hash = sync_state.finalized_head_hash().unwrap_or(block_hash);

        let state = ForkchoiceState {
            head_block_hash: block_hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_hash,
        };

        let fcu_response =
            client.fork_choice_updated_v3(state, None).await.map_err(ProcessorError::Engine)?;

        // Validate FCU response.
        match fcu_response.payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                warn!("FCU after seal returned ACCEPTED status");
            }
        }

        // Update the sync state with the new unsafe head.
        if let Ok(block_info) = client.l2_block_info_by_hash(block_hash) {
            sync_state.set_unsafe_head(block_info);
        }

        Ok(envelope)
    }
}
