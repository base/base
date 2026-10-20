use std::sync::Arc;

use base_builder_publish::WebSocketPublisher;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTimeUpdateTx;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, MAX_DECOMPRESSED_ENVELOPE_BYTES,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BaseBuiltPayload;
use base_node_core::BaseEngineTypes;
use reth_payload_builder_primitives::Events;
use ssz::Encode;
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

/// Publishes payloads resolved by the basic builder in the P2P block encoding.
///
/// Each block is one binary WebSocket frame holding the snappy-compressed SSZ
/// [`BaseExecutionPayloadEnvelope`]: the bytes the sequencer signs for P2P gossip,
/// without the signature.
#[derive(Debug)]
pub struct BlockStreamPublisher {
    /// WebSocket publisher that fans messages out to stream clients.
    pub ws_pub: Arc<WebSocketPublisher>,
    /// Chain spec used to tell whether a block must carry a `BaseTime` timestamp.
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
        let block = payload.block();
        if self.chain_spec.is_denim_active_at_timestamp(block.timestamp)
            && let Err(error) = BaseTimeUpdateTx::extract_timestamp_ms(
                &block.body().transactions,
                block.number,
                block.timestamp,
            )
        {
            metrics::counter!("block_stream_missing_timestamp_ms_total").increment(1);
            warn!(error = %error, block_number = block.number, "Denim block has no BaseTime timestamp");
        }

        let (execution_payload, _) =
            BaseExecutionPayload::from_block_unchecked(block.hash(), &block.clone_block());
        let ssz = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: block.parent_beacon_block_root,
            execution_payload,
        }
        .as_ssz_bytes();
        if ssz.len() > MAX_DECOMPRESSED_ENVELOPE_BYTES {
            metrics::counter!("block_stream_skipped_payloads_total", "reason" => "oversized")
                .increment(1);
            warn!(
                block_number = block.number,
                size_bytes = ssz.len(),
                max_bytes = MAX_DECOMPRESSED_ENVELOPE_BYTES,
                "block exceeds the P2P payload size limit, skipping block"
            );
            return;
        }
        let compressed = match snap::raw::Encoder::new().compress_vec(&ssz) {
            Ok(compressed) => compressed,
            Err(error) => {
                metrics::counter!("block_stream_skipped_payloads_total", "reason" => "serialize")
                    .increment(1);
                error!(error = %error, block_number = block.number, "failed to encode block");
                return;
            }
        };
        self.ws_pub.publish_message(Message::binary(compressed), block.number, 0);
    }
}
