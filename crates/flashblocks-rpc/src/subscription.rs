use std::{io::Read, sync::Arc, time::Instant};

use alloy_primitives::map::foldhash::HashMap;
use futures_util::StreamExt;
use reth_optimism_primitives::OpReceipt;
use rollup_boost::FlashblocksPayloadV1;
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
    pub new_account_balances: HashMap<String, String>, // Address -> Balance (hex)
    pub block_number: u64,
}

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum ActorMessage {
    BestPayload { payload: FlashblocksPayloadV1 },
}

pub struct FlashblocksSubscriber {
    sender: mpsc::Sender<ActorMessage>,
    mailbox: mpsc::Receiver<ActorMessage>,
    flashblocks_state: Arc<FlashblocksState>,
    metrics: Metrics,
    ws_url: Url,
}

impl FlashblocksSubscriber {
    pub fn new(cache: Arc<FlashblocksState>, ws_url: Url) -> Self {
        let (sender, mailbox) = mpsc::channel(100);

        Self {
            sender,
            mailbox,
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
        let sender = self.sender.clone();
        let flashblocks_state = self.flashblocks_state.clone();

        // Take ownership of mailbox for the actor loop
        let mut mailbox = std::mem::replace(&mut self.mailbox, mpsc::channel(1).1);

        // Spawn WebSocket handler with integrated actor loop
        let metrics = self.metrics.clone(); // Clone here for the first spawn
        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(1);
            const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

            loop {
                match connect_async(ws_url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        info!(message = "WebSocket connection established");

                        let (_write, mut read) = ws_stream.split();
                        // Handle incoming messages
                        while let Some(msg) = read.next().await {
                            metrics.upstream_messages.increment(1);
                            let msg_start_time = Instant::now();

                            match msg {
                                Ok(Message::Binary(bytes)) => {
                                    let text = match try_parse_message(&bytes) {
                                        Ok(text) => text,
                                        Err(e) => {
                                            error!(
                                                message = "failed to decode message",
                                                error = %e
                                            );
                                            continue;
                                        }
                                    };

                                    let payload: FlashblocksPayloadV1 =
                                        match serde_json::from_str(&text) {
                                            Ok(m) => m,
                                            Err(e) => {
                                                error!(
                                                    message = "failed to parse message",
                                                    error = %e
                                                );
                                                continue;
                                            }
                                        };

                                    let _ =
                                        sender.send(ActorMessage::BestPayload { payload }).await;
                                    metrics
                                        .websocket_processing_duration
                                        .record(msg_start_time.elapsed());
                                }
                                Ok(Message::Close(_)) => break,
                                Err(e) => {
                                    metrics.upstream_errors.increment(1);
                                    error!(
                                        message = "error receiving message",
                                        error = %e
                                    );
                                    break;
                                }
                                _ => {} // Handle other message types if needed
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

fn try_parse_message(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
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
