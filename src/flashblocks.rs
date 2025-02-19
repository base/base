use crate::cache::Cache;
use alloy_primitives::{map::foldhash::HashMap, Address, B256, U256};
use futures_util::StreamExt;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelopeV3;
use reth::core::primitives::SignedTransaction;
use reth_optimism_primitives::{OpBlock, OpReceipt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
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
        //new_account_balances: HashMap<Address, U256>,
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
        println!("trying to connect to {:?}", url);
        let sender = self.sender.clone();
        let cache_clone = self.cache.clone();

        // Take ownership of mailbox for the actor loop
        let mut mailbox = std::mem::replace(&mut self.mailbox, mpsc::channel(1).1);

        // Spawn WebSocket handler with integrated actor loop
        tokio::spawn(async move {
            loop {
                match connect_async(url.as_str()).await {
                    Ok((ws_stream, _)) => {
                        println!("WebSocket connected!");
                        let (_write, mut read) = ws_stream.split();

                        // Handle incoming messages
                        while let Some(msg) = read.next().await {
                            match msg {
                                Ok(Message::Text(text)) => {
                                    let message: serde_json::Value =
                                        match serde_json::from_str(&text) {
                                            Ok(m) => m,
                                            Err(e) => {
                                                error!("failed to parse message: {}", e);
                                                continue;
                                            }
                                        };
                                    let payload: OpExecutionPayloadEnvelopeV3 =
                                        match serde_json::from_value(message["response"].clone()) {
                                            Ok(p) => p,
                                            Err(e) => {
                                                error!("failed to parse payload: {}", e);
                                                continue;
                                            }
                                        };
                                    let receipts: Vec<OpReceipt> =
                                        match serde_json::from_value(message["receipts"].clone()) {
                                            Ok(r) => r,
                                            Err(e) => {
                                                error!("failed to parse receipts: {}", e);
                                                continue;
                                            }
                                        };
                                    let tx_hashes: Vec<B256> = match serde_json::from_value(
                                        message["tx_hashes"].clone(),
                                    ) {
                                        Ok(h) => h,
                                        Err(e) => {
                                            error!("failed to parse tx_hashes: {}", e);
                                            continue;
                                        }
                                    };
                                    // let new_account_balances: HashMap<Address, U256> = match serde_json::from_value(message["new_account_balances"].clone()) {
                                    //     Ok(b) => b,
                                    //     Err(e) => {
                                    //         error!("failed to parse account balances: {}", e);
                                    //         continue;
                                    //     }
                                    // };

                                    let _ = sender
                                        .send(ActorMessage::BestPayload {
                                            response: payload,
                                            receipts,
                                            tx_hashes,
                                            //new_account_balances,
                                        })
                                        .await;
                                }
                                Ok(Message::Close(_)) => break,
                                Err(e) => {
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
                    ActorMessage::BestPayload {
                        response,
                        receipts,
                        tx_hashes,
                        // new_account_balances,
                    } => {
                        let execution_payload = response.execution_payload;
                        let block: OpBlock = execution_payload
                            .try_into_block()
                            .expect("failed to convert execution payload to block");

                        cache_clone
                            .set(&format!("pending"), &block, Some(10))
                            .expect("failed to set block in cache");
                        println!("block number {:?}", block.number);

                        // Store receipts
                        cache_clone
                            .set(&format!("pending_receipts"), &receipts, Some(10))
                            .expect("failed to set receipts in cache");

                        // Store transactions
                        for tx in block.body.transactions {
                            let tx_hash = *tx.tx_hash();
                            cache_clone
                                .set(&format!("{:?}", tx_hash), &tx, Some(10))
                                .expect("failed to set tx in cache");
                            println!("stored tx {:?}", tx_hash);
                        }

                        // Store tx receipts
                        for (tx_hash, receipt) in tx_hashes.iter().zip(receipts.iter()) {
                            cache_clone
                                .set(&format!("receipt:{:?}", tx_hash), &receipt, Some(10))
                                .expect("failed to set receipt in cache");
                            println!("stored receipt {:?}", tx_hash);
                        }

                        // Store account balances
                        // for (address, balance) in new_account_balances.iter() {
                        //     cache_clone
                        //         .set(&format!("{:?}", address), &balance, Some(10))
                        //         .expect("failed to set account balance in cache");
                        //     println!("stored account balance {:?}", address);
                        // }
                    }
                }
            }
        });

        Ok(())
    }
}
