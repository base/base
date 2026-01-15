//! Engine reset handler.

use alloy_rpc_types_engine::ForkchoiceState;
use base_engine_driver::DirectEngineApi;
use kona_node_service::EngineClientError;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::DirectEngineProcessorError;

/// Handles a reset request.
pub(crate) async fn handle_reset_request<E: DirectEngineApi>(
    client: &E,
    result_tx: mpsc::Sender<Result<(), EngineClientError>>,
) -> Result<(), DirectEngineProcessorError> {
    warn!(target: "direct_engine", "Processing reset request");

    // Get the finalized head to reset to
    let finalized = client
        .l2_finalized_head()
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    if let Some(finalized) = finalized {
        let forkchoice_state = ForkchoiceState {
            head_block_hash: finalized.hash(),
            safe_block_hash: finalized.hash(),
            finalized_block_hash: finalized.hash(),
        };

        let _ = client
            .fork_choice_updated_v3(forkchoice_state, None)
            .await
            .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

        info!(target: "direct_engine", hash = ?finalized.hash(), "Engine reset to finalized head");
    }

    if result_tx.send(Ok(())).await.is_err() {
        warn!(target: "direct_engine", "Failed to send reset result - receiver dropped");
    }

    Ok(())
}
