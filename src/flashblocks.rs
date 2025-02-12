use crate::cache::Cache;
use alloy_primitives::B256;
use futures_util::StreamExt;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelopeV3;
use reth::core::primitives::SignedTransaction;
use reth_optimism_primitives::{OpBlock, OpReceipt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info};
use url::Url;

#[derive(Debug, Deserialize, Serialize)]
struct FlashbotsMessage {
    method: String,
    params: serde_json::Value,
    #[serde(default)]
    id: Option<u64>,
}

// Simplify actor messages to just handle shutdown
#[derive(Debug)]
enum ActorMessage {
    BestPayload {
        response: OpExecutionPayloadEnvelopeV3,
        receipts: Vec<OpReceipt>,
        tx_hashes: Vec<B256>,
    },
}

pub struct FlashblocksClient {
    sender: mpsc::Sender<ActorMessage>,
    mailbox: mpsc::Receiver<ActorMessage>,
    cache: Arc<Cache>,
}

impl FlashblocksClient {
    pub fn new(cache: Arc<Cache>) -> Self {
        let (sender, mailbox) = mpsc::channel(100);

        Self {
            sender,
            mailbox,
            cache,
        }
    }

    pub fn init(&mut self, ws_url: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = Url::parse(&ws_url)?;
        let sender = self.sender.clone();

        // Spawn WebSocket handler
        tokio::spawn(async move {
            loop {
                match connect_websocket(&url, sender.clone()).await {
                    Ok(()) => break,
                    Err(e) => {
                        error!(
                            message = "Flashbots WebSocket connection error, retrying in 5 seconds",
                            error = %e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });

        // Take ownership of mailbox and state for the actor loop
        let mut mailbox = std::mem::replace(&mut self.mailbox, mpsc::channel(1).1);

        let cache_clone = self.cache.clone();
        // Spawn actor's event loop
        tokio::spawn(async move {
            while let Some(message) = mailbox.recv().await {
                match message {
                    ActorMessage::BestPayload {
                        response,
                        receipts,
                        tx_hashes,
                    } => {
                        // add logic to add to cache here
                        let execution_payload = response.execution_payload;
                        let block: OpBlock = execution_payload
                            .try_into_block()
                            .expect("failed to convert execution payload to block");
                        // store the block in cache
                        cache_clone
                            .set(&format!("pending"), &block, Some(10))
                            .expect("failed to set block in cache");
                        println!("block number {:?}", block.number);

                        // store all receipts in cache
                        cache_clone
                            .set(&format!("pending_receipts"), &receipts, Some(10))
                            .expect("failed to set receipts in cache");

                        // store each OpTransactctionSigned in cache
                        for tx in block.body.transactions {
                            let tx_hash = *tx.tx_hash();
                            cache_clone
                                .set(&format!("{:?}", tx_hash), &tx, Some(10))
                                .expect("failed to set tx in cache");
                            println!("stored tx {:?}", tx_hash);
                        }

                        // iterate over tx_hashes and receipts, and store them in cache
                        for (tx_hash, receipt) in tx_hashes.iter().zip(receipts.iter()) {
                            cache_clone
                                .set(&format!("receipt:{:?}", tx_hash), &receipt, Some(10))
                                .expect("failed to set receipt in cache");
                            println!("stored receipt {:?}", tx_hash);
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

async fn connect_websocket(
    url: &Url,
    sender: mpsc::Sender<ActorMessage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(url.as_str()).await?;
    let (_write, mut read) = ws_stream.split();

    info!(message = "Flashbots WebSocket connected", url = %url);

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // extract payload, receipts, and tx_hashes from the message
                let message: serde_json::Value = serde_json::from_str(&text)?;
                let payload: OpExecutionPayloadEnvelopeV3 =
                    serde_json::from_value(message["response"].clone())?;
                let receipts: Vec<OpReceipt> = serde_json::from_value(message["receipts"].clone())?;
                let tx_hashes: Vec<B256> = serde_json::from_value(message["tx_hashes"].clone())?;

                let _ = sender
                    .send(ActorMessage::BestPayload {
                        response: payload,
                        receipts,
                        tx_hashes,
                    })
                    .await;
            }
            Ok(Message::Close(_)) => {
                info!(message = "Received close frame");
                break;
            }
            Err(e) => {
                error!(message = "WebSocket error", error = %e);
                return Err(e.into());
            }
            _ => {} // Ignore other message types
        }
    }

    Ok(())
}
