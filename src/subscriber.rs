use futures_util::StreamExt;
use op_alloy_consensus::{OpBlock, OpTypedTransaction};
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelopeV4;
use reqwest::Client;
use serde_json::Value;
use std::error::Error;

use crate::cache::Cache;

pub struct Subscriber {
    cache: Cache,
    producer_url: String,
}

impl Subscriber {
    pub fn new(cache: Cache, producer_url: String) -> Self {
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

                        let execution_payload_envelope: OpExecutionPayloadEnvelopeV4 =
                            serde_json::from_value(json.clone())?;
                        let execution_payload = execution_payload_envelope.execution_payload;
                        let block: OpBlock = execution_payload.try_into_block()?;
                        // store the block in cache
                        self.cache.set(
                            &format!("block:{}", block.header.number),
                            &block,
                            Some(10),
                        )?;

                        let txs = block.body.transactions;
                        for tx in txs {
                            let tx_hash = tx.tx_hash();
                            let typed_tx = OpTypedTransaction::from(tx);
                            // store the tx in cache
                            self.cache
                                .set(&format!("tx:{}", tx_hash), &typed_tx, Some(10))?;
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
