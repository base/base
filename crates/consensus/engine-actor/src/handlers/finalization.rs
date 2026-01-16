//! L1 finalization handler.

use alloy_rpc_types_engine::ForkchoiceState;
use base_engine_driver::DirectEngineApi;
use kona_protocol::BlockInfo;
use tracing::{debug, info};

use crate::DirectEngineProcessorError;

/// Handles processing of a finalized L1 block.
pub(crate) async fn handle_finalized_l1_block<E: DirectEngineApi>(
    client: &E,
    block: Box<BlockInfo>,
) -> Result<(), DirectEngineProcessorError> {
    info!(
        target: "direct_engine",
        block_number = block.number,
        block_hash = ?block.hash,
        "Processing finalized L1 block"
    );

    // Get the current chain head
    let head = client
        .l2_block_info_latest()
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    if let Some(head) = head {
        // Update finalized head based on L1 finality
        // This is a simplified implementation - full version would track L1 origin
        let forkchoice_state = ForkchoiceState {
            head_block_hash: head.hash(),
            safe_block_hash: head.hash(),
            finalized_block_hash: head.hash(),
        };

        let fcu_result = client
            .fork_choice_updated_v3(forkchoice_state, None)
            .await
            .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

        debug!(target: "direct_engine", ?fcu_result, "Finalized head updated");
    }

    Ok(())
}
