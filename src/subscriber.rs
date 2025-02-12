use crate::cache::Cache;
use alloy_primitives::B256;
use futures_util::StreamExt;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelopeV4;
use reqwest::Client;
use reth::core::primitives::SignedTransaction;
use reth_optimism_primitives::{OpBlock, OpReceipt};
use serde_json::Value;
use std::{error::Error, sync::Arc};

pub struct Subscriber {
    cache: Arc<Cache>,
    producer_url: String,
}

impl Subscriber {
    pub fn new(cache: Arc<Cache>, producer_url: String) -> Self {
        Self {
            cache,
            producer_url,
        }
    }

    pub async fn subscribe_to_sse(&self) -> Result<(), Box<dyn Error>> {
        let client = Client::new();
        let response = client.get(&self.producer_url).send().await?;

        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(data) => {
                    let text = String::from_utf8_lossy(data.as_ref());

                    if text.starts_with("data:") {
                        let json: Value = serde_json::from_str(&text[5..])?;

                        // fetch two keys from json, payload and receipts
                        let payload = json.get("payload").unwrap();
                        let receipts = json.get("receipts").unwrap();
                        let tx_hashes = json.get("tx_hashes").unwrap();

                        let execution_payload_envelope: OpExecutionPayloadEnvelopeV4 =
                            serde_json::from_value(payload.clone())?;
                        let execution_payload = execution_payload_envelope.execution_payload;
                        let block: OpBlock = execution_payload.try_into_block()?;
                        // store the block in cache
                        self.cache.set(&format!("pending"), &block, Some(10))?;
                        println!("block number {:?}", block.number);

                        let receipts: Vec<OpReceipt> = serde_json::from_value(receipts.clone())?;
                        let tx_hashes: Vec<B256> = serde_json::from_value(tx_hashes.clone())?;

                        // store all receipts in cache
                        self.cache
                            .set(&format!("pending_receipts"), &receipts, Some(10))?;

                        // store each OpTransactctionSigned in cache
                        for tx in block.body.transactions {
                            let tx_hash = *tx.tx_hash();
                            self.cache.set(&format!("{:?}", tx_hash), &tx, Some(10))?;
                            println!("stored tx {:?}", tx_hash);
                        }

                        // iterate over tx_hashes and receipts, and store them in cache
                        for (tx_hash, receipt) in tx_hashes.iter().zip(receipts.iter()) {
                            self.cache.set(
                                &format!("receipt:{:?}", tx_hash),
                                &receipt,
                                Some(10),
                            )?;
                            println!("stored receipt {:?}", tx_hash);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error reading chunk: {}", e);
                    return Err(Box::new(e));
                }
            }
        }

        Ok(())
    }
}
