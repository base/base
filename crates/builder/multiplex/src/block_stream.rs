use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_primitives::U256;
use alloy_rpc_types_eth::Header;
use base_builder_publish::WebSocketPublisher;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTimeUpdateTx;
use base_common_rpc_types::{BaseHeaderResponse, BlockStreamPayloadV1};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BaseBuiltPayload;
use base_node_core::BaseEngineTypes;
use reth_payload_builder_primitives::Events;
use reth_payload_primitives::BuiltPayload;
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tracing::{error, info, warn};

/// Publishes payloads resolved by the basic builder as [`BlockStreamPayloadV1`] messages.
#[derive(Debug)]
pub struct BlockStreamPublisher {
    /// WebSocket publisher that fans messages out to stream clients.
    pub ws_pub: Arc<WebSocketPublisher>,
    /// Chain spec used to tell whether a block must carry a millisecond timestamp.
    pub chain_spec: Arc<BaseChainSpec>,
}

impl BlockStreamPublisher {
    /// Creates a block stream publisher.
    pub const fn new(ws_pub: Arc<WebSocketPublisher>, chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { ws_pub, chain_spec }
    }

    /// Publishes every resolved payload until the payload event channel closes.
    pub async fn run(self, mut events: Receiver<Events<BaseEngineTypes>>) {
        loop {
            match events.recv().await {
                Ok(Events::BuiltPayload(payload)) => self.publish(&payload),
                Ok(Events::Attributes(_)) => {}
                Err(RecvError::Lagged(skipped)) => {
                    metrics::counter!("block_stream_lagged_events_total").increment(skipped);
                    warn!(skipped, "block stream payload events lagged");
                }
                Err(RecvError::Closed) => {
                    info!("block stream payload events closed");
                    return;
                }
            }
        }
    }

    /// Publishes one resolved payload, keyed by its block number.
    pub fn publish(&self, payload: &BaseBuiltPayload) {
        let block_number = payload.block().number;
        let Some(message) = self.message(payload) else {
            metrics::counter!("block_stream_skipped_payloads_total", "reason" => "no_execution_output")
                .increment(1);
            warn!(block_number, "built payload has no execution output, skipping block");
            return;
        };
        let json = match serde_json::to_string(&message) {
            Ok(json) => json,
            Err(error) => {
                metrics::counter!("block_stream_skipped_payloads_total", "reason" => "serialize")
                    .increment(1);
                error!(error = %error, block_number, "failed to serialize block stream message");
                return;
            }
        };
        if json.len() > BlockStreamPayloadV1::MAX_MESSAGE_BYTES {
            metrics::counter!("block_stream_skipped_payloads_total", "reason" => "oversized")
                .increment(1);
            warn!(
                block_number,
                size_bytes = json.len(),
                max_bytes = BlockStreamPayloadV1::MAX_MESSAGE_BYTES,
                "block stream message exceeds size limit, skipping block"
            );
            return;
        }
        self.ws_pub.publish_json(json, block_number, 0);
    }

    /// Builds the stream message, or `None` when the payload carries no execution output.
    pub fn message(&self, payload: &BaseBuiltPayload) -> Option<BlockStreamPayloadV1> {
        let executed = payload.executed_block()?;
        let block = payload.block();
        let transactions = &block.body().transactions;
        let timestamp_ms = match BaseTimeUpdateTx::extract_timestamp_ms(
            transactions,
            block.number,
            block.timestamp,
        ) {
            Ok(timestamp_ms) => Some(timestamp_ms),
            Err(error) => {
                if self.chain_spec.is_denim_active_at_timestamp(block.timestamp) {
                    metrics::counter!("block_stream_missing_timestamp_ms_total").increment(1);
                    warn!(
                        error = %error,
                        block_number = block.number,
                        "Denim block has no BaseTime timestamp, streaming without timestampMs"
                    );
                }
                None
            }
        };
        let header = Header::from_consensus(
            block.clone_sealed_header().into(),
            None,
            Some(U256::from(block.rlp_length())),
        );
        Some(BlockStreamPayloadV1 {
            block_number: block.number,
            header: BaseHeaderResponse::with_timestamp_ms(header, timestamp_ms),
            transactions: transactions.iter().map(|tx| tx.encoded_2718().into()).collect(),
            receipts: executed.execution_output.receipts.clone(),
        })
    }
}
