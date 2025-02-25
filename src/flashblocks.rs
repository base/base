use crate::cache::Cache;
use alloy_primitives::map::foldhash::HashMap;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use futures_util::StreamExt;
use reth_optimism_primitives::{OpBlock, OpReceipt};
use rollup_boost::{ExecutionPayloadBaseV1, FlashblocksPayloadV1};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::error;
use url::Url;

use crate::metrics::Metrics;
use std::time::Instant;

#[derive(Debug, Deserialize, Serialize)]
struct FlashbotsMessage {
    method: String,
    params: serde_json::Value,
    #[serde(default)]
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    receipts: HashMap<String, OpReceipt>,
    new_account_balances: HashMap<String, String>, // Address -> Balance (hex)
    block_number: u64,
}

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum ActorMessage {
    BestPayload { payload: FlashblocksPayloadV1 },
}

pub struct FlashblocksClient {
    sender: mpsc::Sender<ActorMessage>,
    mailbox: mpsc::Receiver<ActorMessage>,
    cache: Arc<Cache>,
    metrics: Metrics,
}

impl FlashblocksClient {
    pub fn new(cache: Arc<Cache>) -> Self {
        let (sender, mailbox) = mpsc::channel(100);

        Self {
            sender,
            mailbox,
            cache,
            metrics: Default::default(),
        }
    }

    pub fn init(&mut self, ws_url: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = Url::parse(&ws_url)?;
        println!("trying to connect to {:?}", url);
        let sender = self.sender.clone();
        let cache_clone = self.cache.clone();

        // Take ownership of mailbox for the actor loop
        let mut mailbox = std::mem::replace(&mut self.mailbox, mpsc::channel(1).1);

        // Spawn WebSocket handler with integrated actor loop
        let metrics = self.metrics.clone(); // Take reference before the loop
        tokio::spawn(async move {
            loop {
                match connect_async(url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        println!("WebSocket connected!");
                        let (_write, mut read) = ws_stream.split();
                        // Handle incoming messages
                        while let Some(msg) = read.next().await {
                            metrics.upstream_messages.increment(1);
                            match msg {
                                Ok(Message::Binary(bytes)) => {
                                    // Decode binary message to string first
                                    let text = match String::from_utf8(bytes.to_vec()) {
                                        Ok(text) => text,
                                        Err(e) => {
                                            error!("Failed to decode binary message: {}", e);
                                            continue;
                                        }
                                    };

                                    // Then parse JSON
                                    let payload: FlashblocksPayloadV1 =
                                        match serde_json::from_str(&text) {
                                            Ok(m) => m,
                                            Err(e) => {
                                                error!("failed to parse message: {}", e);
                                                continue;
                                            }
                                        };

                                    let _ =
                                        sender.send(ActorMessage::BestPayload { payload }).await;
                                }
                                Ok(Message::Close(_)) => break,
                                Err(e) => {
                                    metrics.upstream_errors.increment(1);
                                    error!("Error receiving message: {}", e);
                                    break;
                                }
                                _ => {} // Handle other message types if needed
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            message = "WebSocket connection error, retrying in 5 seconds",
                            error = %e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
                        let msg_processing_start_time = Instant::now();
                        let metadata: Metadata = serde_json::from_value(payload.metadata)
                            .expect("failed to deserialize metadata");
                        let receipts = metadata.receipts;
                        let new_account_balances = metadata.new_account_balances;
                        let block_number = metadata.block_number;
                        let diff = payload.diff;
                        let withdrawals = diff.withdrawals.clone();

                        // Skip if index is 0 and base is not cached, likely the first payload
                        if payload.index != 0
                            && cache_clone
                                .get::<ExecutionPayloadBaseV1>(&format!("base:{:?}", block_number))
                                .is_none()
                        {
                            continue;
                        }

                        let base = if let Some(base) = payload.base {
                            cache_clone
                                .set(&format!("base:{:?}", block_number), &base, Some(10))
                                .expect("failed to set base in cache");
                            base
                        } else {
                            cache_clone
                                .get(&format!("base:{:?}", block_number))
                                .expect("failed to get base from cache")
                        };

                        let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
                            blob_gas_used: 0,
                            excess_blob_gas: 0,
                            payload_inner: ExecutionPayloadV2 {
                                withdrawals: withdrawals,
                                payload_inner: ExecutionPayloadV1 {
                                    parent_hash: base.parent_hash,
                                    fee_recipient: base.fee_recipient,
                                    state_root: diff.state_root,
                                    receipts_root: diff.receipts_root,
                                    logs_bloom: diff.logs_bloom,
                                    prev_randao: base.prev_randao,
                                    block_number: base.block_number,
                                    gas_limit: base.gas_limit,
                                    gas_used: diff.gas_used,
                                    timestamp: base.timestamp,
                                    extra_data: base.extra_data,
                                    base_fee_per_gas: base.base_fee_per_gas,
                                    block_hash: diff.block_hash,
                                    transactions: diff.transactions.clone(),
                                },
                            },
                        };

                        let block: OpBlock = execution_payload
                            .try_into_block()
                            .expect("failed to convert execution payload to block");

                        cache_clone
                            .set(&format!("pending"), &block, Some(10))
                            .expect("failed to set block in cache");

                        // Store receipts
                        let all_receipts = receipts.values().cloned().collect::<Vec<_>>();
                        cache_clone
                            .set(&format!("pending_receipts"), &all_receipts, Some(10))
                            .expect("failed to set receipts in cache");

                        // Store tx receipts
                        for (tx_hash, receipt) in receipts.iter() {
                            cache_clone
                                .set(&format!("receipt:{:?}", tx_hash), receipt, Some(10))
                                .expect("failed to set receipt in cache");
                        }

                        // Store account balances
                        for (address, balance) in new_account_balances.iter() {
                            cache_clone
                                .set(&format!("{:?}", address), &balance, Some(10))
                                .expect("failed to set account balance in cache");
                        }

                        metrics
                            .block_processing_duration
                            .record(msg_processing_start_time.elapsed());
                    }
                }
            }
        });

        Ok(())
    }
}
