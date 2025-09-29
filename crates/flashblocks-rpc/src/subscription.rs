use std::time::{SystemTime, UNIX_EPOCH};
use std::{io::Read, sync::Arc, time::Duration};

use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use futures_util::{SinkExt as _, StreamExt};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use crate::metrics::Metrics;

/// Interval of liveness check of upstream, in milliseconds.
pub const PING_INTERVAL_MS: u64 = 500;

/// Max duration of backoff before reconnecting to upstream.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);

pub trait FlashblocksReceiver {
    fn on_flashblock_received(&self, flashblock: Flashblock);
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Metadata {
    pub receipts: HashMap<B256, OpReceipt>,
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

pub struct FlashblocksSubscriber<Receiver> {
    flashblocks_state: Arc<Receiver>,
    metrics: Metrics,
    ws_url: Url,
}

impl<Receiver> FlashblocksSubscriber<Receiver>
where
    Receiver: FlashblocksReceiver + Send + Sync + 'static,
{
    pub fn new(flashblocks_state: Arc<Receiver>, ws_url: Url) -> Self {
        Self {
            ws_url,
            flashblocks_state,
            metrics: Metrics::default(),
        }
    }

    pub fn start(&mut self) {
        info!(
            message = "Starting Flashblocks subscription",
            url = %self.ws_url,
        );

        let ws_url = self.ws_url.clone();

        let (sender, mut mailbox) = mpsc::channel(100);

        let metrics = self.metrics.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                match connect_async(ws_url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        info!(message = "WebSocket connection established");

                        let mut ping_interval = interval(Duration::from_millis(PING_INTERVAL_MS));
                        let mut awaiting_pong_resp = false;

                        let (mut write, mut read) = ws_stream.split();

                        'conn: loop {
                            tokio::select! {
                                Some(msg) = read.next() => {
                                    metrics.upstream_messages.increment(1);

                                    match msg {
                                        Ok(Message::Binary(bytes)) => match try_decode_message(&bytes) {
                                            Ok(payload) => {
                                                let _ = sender.send(ActorMessage::BestPayload { payload: payload.clone() }).await.map_err(|e| {
                                                    error!(message = "Failed to publish message to channel", error = %e);
                                                });
                                            }
                                            Err(e) => {
                                                error!(
                                                    message = "error decoding flashblock message",
                                                    error = %e
                                                );
                                            }
                                        },
                                        Ok(Message::Text(_)) => {
                                            error!("Received flashblock as plaintext, only compressed flashblocks supported. Set up websocket-proxy to use compressed flashblocks.");
                                        }
                                        Ok(Message::Close(_)) => {
                                            info!(message = "WebSocket connection closed by upstream");
                                            break;
                                        }
                                        Ok(Message::Pong(data)) => {
                                            trace!(target: "flashblocks_rpc::subscription",
                                                ?data,
                                                "Received pong from upstream"
                                            );
                                            awaiting_pong_resp = false;
                                            if let Some(rtt_ms) = rtt_from_pong(data.as_ref()) {
                                                debug!(
                                                    message= "Received pong from upstream flashblocks",
                                                    rtt_ms = rtt_ms,
                                                );
                                            }else {
                                                debug!(
                                                    message = "Received UNEXPECTED pong from upstream flashblocks",
                                                    data=?data
                                                );
                                            }
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
                                },
                                _ = ping_interval.tick() => {
                                    if awaiting_pong_resp {
                                          warn!(
                                            target: "flashblocks_rpc::subscription",
                                            ?backoff,
                                            timeout_ms = PING_INTERVAL_MS,
                                            "No pong response from upstream, reconnecting",
                                        );

                                        backoff = sleep(&metrics, backoff).await;
                                        break 'conn;
                                    }

                                    trace!(target: "flashblocks_rpc::subscription",
                                        "Sending ping to upstream"
                                    );

                                    if let Err(error) = write.send(ping_with_timestamp()).await {
                                        warn!(
                                            target: "flashblocks_rpc::subscription",
                                            ?backoff,
                                            %error,
                                            "WebSocket connection lost, reconnecting",
                                        );

                                        backoff = sleep(&metrics, backoff).await;
                                        break 'conn;
                                    }
                                    awaiting_pong_resp = true
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            message = "WebSocket connection error, retrying",
                            backoff_duration = ?backoff,
                            error = %e
                        );

                        backoff = sleep(&metrics, backoff).await;
                        continue;
                    }
                }
            }
        });

        let flashblocks_state = self.flashblocks_state.clone();
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

/// Sleeps for given backoff duration. Returns incremented backoff duration, capped at [`MAX_BACKOFF`].
async fn sleep(metrics: &Metrics, backoff: Duration) -> Duration {
    metrics.reconnect_attempts.increment(1);
    tokio::time::sleep(backoff).await;
    std::cmp::min(backoff * 2, MAX_BACKOFF)
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

fn ping_with_timestamp() -> Message {
    let now_us: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);

    Message::Ping(now_us.to_be_bytes().to_vec().into())
}

fn rtt_from_pong(data: &[u8]) -> Option<f64> {
    if data.len() == 8 {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&data[..8]);
        let sent_us = u64::from_be_bytes(arr);

        let now_us: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);

        Some(now_us.saturating_sub(sent_us) as f64 / 1000.0)
    } else {
        None
    }
}
