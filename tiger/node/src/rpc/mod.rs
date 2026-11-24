//! JSON-RPC methods on the HTTP port (and the auth port): block, transaction and state reads.

use std::sync::Arc;

use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, B256, U64, U256};
use alloy_rpc_types_eth::TransactionInfo;
use base_common_consensus::{BaseTransactionInfo, DepositInfo};
use serde_json::{Value, json};

use crate::{Node, RpcError, chain::StoredBlock};

mod blocks;
mod receipts;

fn param<T: serde::de::DeserializeOwned>(params: &Value, index: usize) -> Result<T, RpcError> {
    let value = params.get(index).cloned().unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|error| RpcError::invalid_params(format!("invalid params: {error}")))
}

impl Node {
    /// Answers one non-Engine JSON-RPC method.
    pub fn rpc(&self, method: &str, p: &Value) -> Result<Value, RpcError> {
        if let Some(result) = self.rpc_blocks(method, p) {
            return result;
        }
        match method {
            "rpc_modules" => Ok(json!({
                "debug": "1.0", "web3": "1.0", "rpc": "1.0", "txpool": "1.0", "trace": "1.0", "net": "1.0", "eth": "1.0"
            })),
            "eth_chainId" => Ok(json!(U64::from(self.spec.chain_id))),
            "net_version" => Ok(json!(self.spec.chain_id.to_string())),
            "web3_clientVersion" => Ok(json!(concat!("tiger/v", env!("CARGO_PKG_VERSION")))),
            "eth_syncing" => Ok(json!(false)),
            "eth_blockNumber" => {
                Ok(json!(U64::from(self.chain.read().expect("chain lock").head().number())))
            }
            "eth_getBlockByNumber" | "eth_getBlockByHash" => {
                let full: bool = param(p, 1)?;
                Ok(self.find_block(p.get(0))?.map_or(Value::Null, |block| self.block_json(&block, full)))
            }
            "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" => {
                let address: Address = param(p, 0)?;
                let block = self.state_block(p.get(1))?;
                let chain = self.chain.read().expect("chain lock");
        let state = chain.state_after(&block).map_err(RpcError::internal)?;
                let account = state.account(&address).map_err(|error| RpcError::internal(error.to_string()))?;
                Ok(match method {
                    "eth_getBalance" => json!(account.map_or(U256::ZERO, |account| account.balance)),
                    "eth_getTransactionCount" => json!(U64::from(account.map_or(0, |account| account.nonce))),
                    _ => {
                        let code = match account {
                            Some(account) => state
                                .code(&account.code_hash)
                                .map_err(|error| RpcError::internal(error.to_string()))?,
                            None => Default::default(),
                        };
                        json!(code)
                    }
                })
            }
            "eth_getStorageAt" => {
                let address: Address = param(p, 0)?;
                let slot: U256 = param(p, 1)?;
                let block = self.state_block(p.get(2))?;
                let chain = self.chain.read().expect("chain lock");
        let state = chain.state_after(&block).map_err(RpcError::internal)?;
                let value = state.storage(&address, &slot).map_err(|error| RpcError::internal(error.to_string()))?;
                Ok(json!(B256::from(value)))
            }
            "eth_getProof" => {
                let address: Address = param(p, 0)?;
                let slots: Vec<U256> = param(p, 1)?;
                let block = self.state_block(p.get(2))?;
                self.proof(&block, address, &slots)
            }
            "eth_getTransactionByHash" => {
                let hash: B256 = param(p, 0)?;
                let chain = self.chain.read().expect("chain lock");
                let Some((number, index)) = chain.transaction(&hash) else { return Ok(Value::Null) };
                let block = chain.canonical_block(number).expect("indexed transactions are canonical");
                Ok(self.transaction_json(&block, index))
            }
            "eth_getTransactionReceipt" => {
                let hash: B256 = param(p, 0)?;
                let block = {
                    let chain = self.chain.read().expect("chain lock");
                    let Some((number, index)) = chain.transaction(&hash) else { return Ok(Value::Null) };
                    (chain.canonical_block(number).expect("indexed transactions are canonical"), index)
                };
                self.transaction_receipt(&block.0, block.1)
            }
            "eth_getBlockReceipts" => match self.find_block(p.get(0))? {
                Some(block) => Ok(Value::Array(self.block_receipts(&block)?)),
                None => Ok(Value::Null),
            },
            #[cfg(feature = "edge-coverage")]
            "fuzz_coverage" => {
                let edges = crate::EdgeCounters::with_edges(Default::default());
                crate::EdgeCounters::reset();
                Ok(json!(edges))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    /// The block a tag, number, hash or `{blockHash, requireCanonical}` object names; `None`
    /// for an unknown block. A missing tag is `latest`.
    pub fn find_block(&self, tag: Option<&Value>) -> Result<Option<Arc<StoredBlock>>, RpcError> {
        let chain = self.chain.read().expect("chain lock");
        let forkchoice = chain.forkchoice();
        let by_hash = |hash: B256, canonical: bool| {
            chain.block(&hash).filter(|block| !canonical || chain.is_canonical(&block.hash))
        };
        Ok(match tag {
            None | Some(Value::Null) => Some(chain.head()),
            Some(Value::String(text)) => match text.as_str() {
                "latest" | "pending" => Some(chain.head()),
                "earliest" => chain.canonical_block(chain.genesis.header.number),
                "safe" => chain.block(&forkchoice.safe),
                "finalized" => chain.block(&forkchoice.finalized),
                text if text.len() == 66 => {
                    let hash: B256 = text.parse().map_err(|_| RpcError::invalid_params(format!("invalid block hash {text}")))?;
                    by_hash(hash, false)
                }
                text => {
                    let number: U64 = serde_json::from_value(json!(text))
                        .map_err(|_| RpcError::invalid_params(format!("invalid block tag {text}")))?;
                    chain.canonical_block(number.to())
                }
            },
            Some(Value::Object(object)) => {
                if let Some(hash) = object.get("blockHash") {
                    let hash: B256 = serde_json::from_value(hash.clone())
                        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
                    let canonical = object.get("requireCanonical").and_then(Value::as_bool).unwrap_or(false);
                    by_hash(hash, canonical)
                } else if let Some(number) = object.get("blockNumber") {
                    let number: U64 = serde_json::from_value(number.clone())
                        .map_err(|error| RpcError::invalid_params(error.to_string()))?;
                    chain.canonical_block(number.to())
                } else {
                    return Err(RpcError::invalid_params("invalid block id".into()));
                }
            }
            Some(other) => return Err(RpcError::invalid_params(format!("invalid block id {other}"))),
        })
    }

    /// The block whose post-state a state read uses; an unknown block is an error.
    pub fn state_block(&self, tag: Option<&Value>) -> Result<Arc<StoredBlock>, RpcError> {
        self.find_block(tag)?.ok_or_else(|| RpcError::new(-32001, "block not found"))
    }

    /// `eth_getProof` at the post-state of `block`.
    fn proof(&self, block: &StoredBlock, address: Address, slots: &[U256]) -> Result<Value, RpcError> {
        let chain = self.chain.read().expect("chain lock");
        let state = chain.state_after(block).map_err(RpcError::internal)?;
        let proof = state.proof(address, slots).map_err(RpcError::internal)?;
        let storage_proof: Vec<Value> = slots
            .iter()
            .zip(&proof.storage)
            .map(|(slot, storage)| json!({ "key": B256::from(*slot), "value": storage.value, "proof": storage.proof }))
            .collect();
        let account = proof.account;
        Ok(json!({
            "address": address,
            "accountProof": proof.proof,
            "balance": account.map_or(U256::ZERO, |account| account.balance),
            "codeHash": account.and_then(|account| account.bytecode_hash).unwrap_or(alloy_primitives::KECCAK256_EMPTY),
            "nonce": U64::from(account.map_or(0, |account| account.nonce)),
            "storageHash": proof.storage_root,
            "storageProof": storage_proof,
        }))
    }

    /// The RPC form of transaction `index` of `block`.
    pub fn transaction_json(&self, block: &StoredBlock, index: usize) -> Value {
        let tx = block.transactions[index].clone();
        let receipt = &block.receipts[index];
        let info = BaseTransactionInfo::new(
            TransactionInfo {
                hash: Some(block.tx_hashes[index]),
                index: Some(index as u64),
                block_hash: Some(block.hash),
                block_number: Some(block.number()),
                block_timestamp: Some(block.header.timestamp),
                base_fee: block.header.base_fee_per_gas,
            },
            DepositInfo {
                deposit_nonce: receipt.deposit_nonce(),
                deposit_receipt_version: receipt.deposit_receipt_version(),
            },
        );
        let recovered = Recovered::new_unchecked(tx, block.senders[index]);
        let mut transaction = base_common_rpc_types::Transaction::from_transaction(recovered, info);
        transaction.block_timestamp_ms = Self::timestamp_ms(block);
        json!(transaction)
    }

    /// The RPC form of `block`, with full transactions or their hashes.
    pub fn block_json(&self, block: &StoredBlock, full: bool) -> Value {
        let header = &block.header;
        let size = alloy_rlp::Encodable::length(&Self::consensus_block(block));
        let transactions: Vec<Value> = if full {
            (0..block.transactions.len()).map(|index| self.transaction_json(block, index)).collect()
        } else {
            block.tx_hashes.iter().map(|hash| json!(hash)).collect()
        };
        let mut value = json!(alloy_rpc_types_eth::Header {
            hash: block.hash,
            inner: header.clone(),
            total_difficulty: None,
            size: Some(U256::from(size)),
        });
        let object = value.as_object_mut().expect("headers serialize to objects");
        if let Some(timestamp_ms) = Self::timestamp_ms(block) {
            object.insert("timestampMs".into(), json!(U64::from(timestamp_ms)));
        }
        object.insert("uncles".into(), json!([]));
        object.insert("transactions".into(), Value::Array(transactions));
        if header.withdrawals_root.is_some() {
            object.insert("withdrawals".into(), json!([]));
        }
        value
    }
}
