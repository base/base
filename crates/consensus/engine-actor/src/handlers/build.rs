//! Build request handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadId};
use base_engine_driver::DirectEngineApi;
use kona_protocol::OpAttributesWithParent;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::DirectEngineProcessorError;

/// Handles a build request by calling fork_choice_updated with attributes.
pub(crate) async fn handle_build_request<E: DirectEngineApi>(
    client: &E,
    attributes: Box<OpAttributesWithParent>,
    result_tx: mpsc::Sender<PayloadId>,
) -> Result<(), DirectEngineProcessorError> {
    let parent = &attributes.parent;
    info!(
        target: "direct_engine",
        parent_hash = ?parent.hash(),
        parent_number = parent.block_info.number,
        "Processing build request"
    );

    // Create forkchoice state with parent as head
    let forkchoice_state = ForkchoiceState {
        head_block_hash: parent.hash(),
        safe_block_hash: parent.hash(),
        finalized_block_hash: parent.hash(),
    };

    // Call fork_choice_updated_v3 with attributes to start block building
    let result = client
        .fork_choice_updated_v3(forkchoice_state, Some(attributes.attributes.clone()))
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    // Extract payload ID from result
    if let Some(payload_id) = result.payload_id {
        info!(target: "direct_engine", %payload_id, "Block building started");
        if result_tx.send(payload_id).await.is_err() {
            warn!(target: "direct_engine", "Failed to send payload ID - receiver dropped");
        }
    } else {
        error!(
            target: "direct_engine",
            status = ?result.payload_status.status,
            "Fork choice update did not return payload ID"
        );
        return Err(DirectEngineProcessorError::EngineApi(
            "No payload ID returned from fork_choice_updated".into(),
        ));
    }

    Ok(())
}
