//! Derived attributes (consolidation) handler.

use alloy_rpc_types_engine::ForkchoiceState;
use base_engine_driver::DirectEngineApi;
use kona_protocol::OpAttributesWithParent;
use tracing::{debug, info};

use crate::DirectEngineProcessorError;

/// Handles consolidation of derived L2 attributes with the unsafe chain.
pub(crate) async fn handle_derived_attributes<E: DirectEngineApi>(
    client: &E,
    attributes: Box<OpAttributesWithParent>,
) -> Result<(), DirectEngineProcessorError> {
    let parent = &attributes.parent;
    info!(
        target: "direct_engine",
        parent_hash = ?parent.hash(),
        parent_number = parent.block_info.number,
        "Processing derived L2 attributes (consolidation)"
    );

    // In consolidation, we check if the unsafe block matches the derived attributes
    // If not, we need to reorg. For now, just update the safe head.
    let forkchoice_state = ForkchoiceState {
        head_block_hash: parent.hash(),
        safe_block_hash: parent.hash(),
        finalized_block_hash: parent.hash(),
    };

    let fcu_result = client
        .fork_choice_updated_v3(forkchoice_state, None)
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    debug!(target: "direct_engine", ?fcu_result, "Safe head updated after consolidation");

    Ok(())
}
