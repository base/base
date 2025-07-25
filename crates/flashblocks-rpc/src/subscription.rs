use std::{io::Read, sync::Arc};

use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_engine::PayloadId;
use futures_util::StreamExt;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info};
use url::Url;

use crate::metrics::Metrics;
use crate::state::FlashblocksState;

#[derive(Debug, Deserialize, Serialize)]
struct FlashbotsMessage {
    method: String,
    params: serde_json::Value,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Metadata {
    pub receipts: HashMap<String, OpReceipt>,
    pub new_account_balances: HashMap<Address, U256>,
    pub block_number: u64,
}

#[derive(Debug, Clone)]
pub struct Flashblock {
    pub payload_id: PayloadId,
    pub index: u64,
    pub base: Option<ExecutionPayloadBaseV1>,
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    pub metadata: Metadata,
}

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum ActorMessage {
    BestPayload { payload: Flashblock },
}

pub struct FlashblocksSubscriber {
    flashblocks_state: Arc<FlashblocksState>,
    metrics: Metrics,
    ws_url: Url,
}

impl FlashblocksSubscriber {
    pub fn new(cache: Arc<FlashblocksState>, ws_url: Url) -> Self {
        Self {
            ws_url,
            flashblocks_state: cache,
            metrics: Metrics::default(),
        }
    }

    pub fn start(&mut self) {
        info!(
            message = "Starting Flashblocks subscription",
            url = %self.ws_url,
        );

        let ws_url = self.ws_url.clone();
        let flashblocks_state = self.flashblocks_state.clone();

        let (sender, mut mailbox) = mpsc::channel(100);

        // Spawn WebSocket handler with integrated actor loop
        let metrics = self.metrics.clone(); // Clone here for the first spawn
        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(1);
            const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

            loop {
                match connect_async(ws_url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        info!(message = "WebSocket connection established");

                        let (_, mut read) = ws_stream.split();

                        while let Some(msg) = read.next().await {
                            metrics.upstream_messages.increment(1);

                            match msg {
                                Ok(Message::Binary(bytes)) => match try_decode_message(&bytes) {
                                    Ok(payload) => {
                                        let _ = sender.send(ActorMessage::BestPayload { payload: payload.clone() }).await.map_err(|e| {
                                            error!(message = "Failed to publish message to chanell", error = %e);
                                        });
                                    }
                                    Err(e) => {
                                        error!(
                                            message = "error decoding flashblock message",
                                            error = %e
                                        );
                                    }
                                },
                                Ok(Message::Close(_)) => {
                                    info!(message = "WebSocket connection closed by upstream");
                                    break;
                                }
                                Err(e) => {
                                    metrics.upstream_errors.increment(1);
                                    error!(
                                        message = "error receiving message",
                                        error = %e
                                    );
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            message = "WebSocket connection error, retrying",
                            backoff_duration = ?backoff,
                            error = %e
                        );
                        tokio::time::sleep(backoff).await;
                        // Double the backoff time, but cap at MAX_BACKOFF
                        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                        continue;
                    }
                }
            }
        });

        // Spawn actor's event loop
        tokio::spawn(async move {
            while let Some(message) = mailbox.recv().await {
                match message {
                    ActorMessage::BestPayload { payload } => {
                        flashblocks_state.on_flashblock_received(payload);
                    }
                }
            }
        });
    }
}

fn try_decode_message(bytes: &[u8]) -> eyre::Result<Flashblock> {
    let text = try_parse_message(bytes)?;

    let payload: FlashblocksPayloadV1 = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            return Err(eyre::eyre!("failed to parse message: {}", e));
        }
    };

    let metadata: Metadata = match serde_json::from_value(payload.metadata.clone()) {
        Ok(m) => m,
        Err(e) => {
            return Err(eyre::eyre!("failed to parse message metadata: {}", e));
        }
    };

    Ok(Flashblock {
        payload_id: payload.payload_id,
        index: payload.index,
        base: payload.base,
        diff: payload.diff,
        metadata,
    })
}

fn try_parse_message(bytes: &[u8]) -> eyre::Result<String> {
    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
        if text.trim_start().starts_with("{") {
            return Ok(text);
        }
    }

    let mut decompressor = brotli::Decompressor::new(bytes, 4096);
    let mut decompressed = Vec::new();
    decompressor.read_to_end(&mut decompressed)?;

    let text = String::from_utf8(decompressed)?;
    Ok(text)
}
