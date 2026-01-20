//! Unsafe block processing handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use base_engine_ext::DirectEngineApi;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelopeV3;
use tracing::{debug, warn};

use crate::{EngineSyncState, ProcessorError};

/// Handler for unsafe block (ProcessUnsafeL2Block) requests.
#[derive(Debug)]
pub struct UnsafeBlockHandler;

impl UnsafeBlockHandler {
    /// Handles an unsafe L2 block by inserting it via `new_payload`.
    ///
    /// This is used for blocks received from gossip that haven't been derived yet.
    pub async fn handle<E>(
        client: &E,
        sync_state: &EngineSyncState,
        envelope: OpExecutionPayloadEnvelopeV3,
    ) -> Result<(), ProcessorError>
    where
        E: DirectEngineApi,
    {
        let block_hash = envelope.execution_payload.payload_inner.payload_inner.block_hash;
        let block_number = envelope.execution_payload.payload_inner.payload_inner.block_number;

        debug!(?block_hash, block_number, "Handling unsafe block request");

        // Insert the payload via new_payload.
        let status = client
            .new_payload_v3(
                envelope.execution_payload,
                vec![], // No blob versioned hashes for OP.
                envelope.parent_beacon_block_root,
            )
            .await
            .map_err(ProcessorError::engine)?;

        // Validate payload status.
        match status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                // ACCEPTED is fine for unsafe blocks that extend the chain.
                debug!("new_payload returned ACCEPTED for unsafe block");
            }
        }

        // Update fork choice to make the block the new unsafe head.
        let safe_hash = sync_state.safe_head_hash().unwrap_or(block_hash);
        let finalized_hash = sync_state.finalized_head_hash().unwrap_or(block_hash);

        let state = ForkchoiceState {
            head_block_hash: block_hash,
            safe_block_hash: safe_hash,
            finalized_block_hash: finalized_hash,
        };

        let response =
            client.fork_choice_updated_v3(state, None).await.map_err(ProcessorError::engine)?;

        // Validate FCU response.
        match response.payload_status.status {
            PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing => {}
            PayloadStatusEnum::Invalid { validation_error } => {
                return Err(ProcessorError::InvalidPayloadStatus(validation_error));
            }
            PayloadStatusEnum::Accepted => {
                warn!("FCU for unsafe block returned ACCEPTED status");
            }
        }

        // Update the sync state with the new unsafe head.
        if let Ok(Some(block_info)) = client.l2_block_info_by_hash(block_hash).await {
            sync_state.set_unsafe_head(block_info);
        }

        Ok(())
    }
}
