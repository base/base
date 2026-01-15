//! Unsafe block processing handler.

use alloy_rpc_types_engine::{ExecutionPayloadV3, ForkchoiceState};
use base_engine_driver::DirectEngineApi;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope;
use tracing::{debug, info};

use crate::DirectEngineProcessorError;

/// Handles processing of an unsafe L2 block from P2P.
pub(crate) async fn handle_unsafe_block<E: DirectEngineApi>(
    client: &E,
    envelope: Box<OpExecutionPayloadEnvelope>,
) -> Result<(), DirectEngineProcessorError> {
    let block_hash = envelope.execution_payload.block_hash();
    info!(
        target: "direct_engine",
        ?block_hash,
        "Processing unsafe L2 block"
    );

    // Insert the payload via new_payload
    // For unsafe blocks, we use v3 since they come from P2P without execution requests
    // Extract V3 payload from the OpExecutionPayload enum
    let payload_v3: ExecutionPayloadV3 = match &envelope.execution_payload {
        op_alloy_rpc_types_engine::OpExecutionPayload::V3(p) => p.clone(),
        op_alloy_rpc_types_engine::OpExecutionPayload::V4(p) => p.payload_inner.clone(),
        _ => {
            // V1/V2 payloads - convert to V3 with defaults
            let v2 = envelope.execution_payload.as_v2().unwrap();
            ExecutionPayloadV3 { payload_inner: v2.clone(), blob_gas_used: 0, excess_blob_gas: 0 }
        }
    };

    let payload_status = client
        .new_payload_v3(
            payload_v3,
            vec![], // OP Stack doesn't use blob versioned hashes
            envelope.parent_beacon_block_root.unwrap_or_default(),
        )
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    debug!(target: "direct_engine", ?payload_status, "Unsafe block inserted");

    // Update forkchoice to set this as the head (but not safe/finalized)
    let forkchoice_state = ForkchoiceState {
        head_block_hash: block_hash,
        safe_block_hash: envelope.execution_payload.parent_hash(),
        finalized_block_hash: envelope.execution_payload.parent_hash(),
    };

    let fcu_result = client
        .fork_choice_updated_v3(forkchoice_state, None)
        .await
        .map_err(|e| DirectEngineProcessorError::EngineApi(e.to_string()))?;

    debug!(target: "direct_engine", ?fcu_result, "Fork choice updated for unsafe block");

    Ok(())
}
