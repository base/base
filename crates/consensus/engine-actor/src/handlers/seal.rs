//! Seal request handler.

use alloy_rpc_types_engine::{ForkchoiceState, PayloadId};
use base_engine_driver::DirectEngineApi;
use kona_engine::SealTaskError;
use kona_protocol::OpAttributesWithParent;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::DirectEngineProcessorError;

/// Handles a seal request by getting the payload and inserting it.
pub(crate) async fn handle_seal_request<E: DirectEngineApi>(
    client: &E,
    payload_id: PayloadId,
    attributes: Box<OpAttributesWithParent>,
    result_tx: mpsc::Sender<Result<OpExecutionPayloadEnvelope, SealTaskError>>,
) -> Result<(), DirectEngineProcessorError> {
    info!(
        target: "direct_engine",
        %payload_id,
        parent_hash = ?attributes.parent.hash(),
        "Processing seal request"
    );

    // Get the built payload (using V3 which is compatible with OpExecutionPayloadEnvelope)
    let envelope = match client.get_payload_v3(payload_id).await {
        Ok(env) => env,
        Err(e) => {
            error!(target: "direct_engine", %e, "Failed to get payload");
            // SealTaskError doesn't have a generic Other variant, so we use DepositOnlyPayloadFailed
            // as a fallback for general errors
            let _ = result_tx.send(Err(SealTaskError::DepositOnlyPayloadFailed)).await;
            return Ok(());
        }
    };

    // Block hash is at payload_inner (V2) -> payload_inner (V1) -> block_hash
    let block_hash = envelope.execution_payload.payload_inner.payload_inner.block_hash;
    info!(target: "direct_engine", ?block_hash, "Retrieved built payload");

    // Insert the payload via new_payload
    let payload_status = client
        .new_payload_v3(
            envelope.execution_payload.clone(),
            vec![], // OP Stack doesn't use blob versioned hashes
            attributes.attributes.payload_attributes.parent_beacon_block_root.unwrap_or_default(),
        )
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    debug!(target: "direct_engine", ?payload_status, "Payload inserted");

    // Update forkchoice to make this block the head
    let forkchoice_state = ForkchoiceState {
        head_block_hash: block_hash,
        safe_block_hash: attributes.parent.hash(),
        finalized_block_hash: attributes.parent.hash(),
    };

    let fcu_result = client
        .fork_choice_updated_v3(forkchoice_state, None)
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    debug!(target: "direct_engine", ?fcu_result, "Fork choice updated after seal");

    // Convert V3 envelope to OpExecutionPayloadEnvelope
    let result_envelope = OpExecutionPayloadEnvelope {
        parent_beacon_block_root: Some(envelope.parent_beacon_block_root),
        execution_payload: op_alloy_rpc_types_engine::OpExecutionPayload::V3(
            envelope.execution_payload,
        ),
    };

    if result_tx.send(Ok(result_envelope)).await.is_err() {
        warn!(target: "direct_engine", "Failed to send seal result - receiver dropped");
    }

    Ok(())
}
