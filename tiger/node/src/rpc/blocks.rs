//! Block and transaction lookups by block and index, raw encodings, uncles, and
//! `eth_getTransactionBySenderAndNonce`.

use std::sync::Arc;

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U64, U256};
use alloy_rpc_types_eth::Index;
use serde_json::{Value, json};

use super::param;
use crate::{Node, RpcError, chain::StoredBlock};

impl Node {
    /// Answers the block-and-index, raw-encoding and uncle methods; `None` for other methods.
    pub(super) fn rpc_blocks(&self, method: &str, p: &Value) -> Option<Result<Value, RpcError>> {
        Some(match method {
            "eth_getBlockTransactionCountByNumber" | "eth_getBlockTransactionCountByHash" => {
                self.with_block(method, p, |block| json!(U256::from(block.transactions.len())))
            }
            "eth_getUncleCountByBlockNumber" | "eth_getUncleCountByBlockHash" => {
                self.with_block(method, p, |_| json!(U256::ZERO))
            }
            "eth_getUncleByBlockNumberAndIndex" | "eth_getUncleByBlockHashAndIndex" => {
                self.block_param(method, p).and_then(|_| param::<Index>(p, 1)).map(|_| Value::Null)
            }
            "eth_getTransactionByBlockHashAndIndex" | "eth_getTransactionByBlockNumberAndIndex" => {
                self.block_and_index(method, p).map(|found| {
                    found.map_or(Value::Null, |(block, index)| self.transaction_json(&block, index))
                })
            }
            "eth_getRawTransactionByBlockHashAndIndex" | "eth_getRawTransactionByBlockNumberAndIndex" => {
                self.block_and_index(method, p).map(|found| {
                    found.map_or(Value::Null, |(block, index)| {
                        json!(Bytes::from(block.transactions[index].encoded_2718()))
                    })
                })
            }
            "eth_getRawTransactionByHash" | "debug_getRawTransaction" => param::<B256>(p, 0).map(|hash| {
                let chain = self.chain.read().expect("chain lock");
                chain.transaction(&hash).map_or(Value::Null, |(number, index)| {
                    let block = chain.canonical_block(number).expect("indexed transactions are canonical");
                    json!(Bytes::from(block.transactions[index].encoded_2718()))
                })
            }),
            "debug_getRawHeader" => self.raw_block_param(p).map(|block| json!(Bytes::from(alloy_rlp::encode(&block.header)))),
            "debug_getRawBlock" => self.raw_block_param(p).map(|block| json!(Bytes::from(alloy_rlp::encode(Self::consensus_block(&block))))),
            "debug_getRawReceipts" => self.raw_block_param(p).map(|block| {
                json!(block.receipts.iter().map(|receipt| Bytes::from(receipt.encoded_2718())).collect::<Vec<_>>())
            }),
            "eth_getTransactionBySenderAndNonce" => {
                param::<Address>(p, 0).and_then(|sender| Ok((sender, param::<U64>(p, 1)?.to::<u64>()))).and_then(
                    |(sender, nonce)| self.transaction_by_sender_and_nonce(sender, nonce),
                )
            }
            _ => return None,
        })
    }

    /// `f` of the block the first parameter names, or `null` for an unknown block.
    fn with_block(&self, method: &str, p: &Value, f: impl FnOnce(&StoredBlock) -> Value) -> Result<Value, RpcError> {
        Ok(self.block_param(method, p)?.map_or(Value::Null, |block| f(&block)))
    }

    /// The block the first parameter names: a bare hash for `*Hash*` methods, otherwise a
    /// number or tag.
    fn block_param(&self, method: &str, p: &Value) -> Result<Option<Arc<StoredBlock>>, RpcError> {
        self.block_by_hash_or_tag(method.contains("Hash"), p.get(0))
    }

    /// The block (first parameter) and an in-range transaction index (second parameter).
    fn block_and_index(&self, method: &str, p: &Value) -> Result<Option<(Arc<StoredBlock>, usize)>, RpcError> {
        let block = self.block_param(method, p)?;
        let index: usize = param::<Index>(p, 1)?.into();
        let Some(block) = block else { return Ok(None) };
        Ok((index < block.transactions.len()).then_some((block, index)))
    }

    /// The block a raw-encoding method names; unknown is `-32001`.
    fn raw_block_param(&self, p: &Value) -> Result<Arc<StoredBlock>, RpcError> {
        // The provider has no pending block: base-reth's raw methods do not find one.
        if p.get(0).and_then(Value::as_str) == Some("pending") {
            return Err(RpcError::new(-32001, "block not found: pending"));
        }
        // An unset `safe` or `finalized` fails reth's block number conversion first.
        for tag in ["safe", "finalized"] {
            if p.get(0).and_then(Value::as_str) == Some(tag) && self.find_block(p.get(0))?.is_none() {
                return Err(RpcError::internal(format!("{tag} block does not exist")));
            }
        }
        self.find_block(p.get(0))?.ok_or_else(|| Self::block_not_found(p.get(0)))
    }

    /// The consensus block (header and body) of `block`.
    pub fn consensus_block(
        block: &StoredBlock,
    ) -> alloy_consensus::Block<base_common_consensus::BaseTxEnvelope> {
        alloy_consensus::Block {
            header: block.header.clone(),
            body: alloy_consensus::BlockBody {
                transactions: block.transactions.clone(),
                ommers: Vec::new(),
                withdrawals: block.header.withdrawals_root.map(|_| Default::default()),
            },
        }
    }

    /// The nonce of `sender` after canonical block `number`; an unknown block is `-32001`.
    fn nonce_at(&self, sender: Address, number: u64) -> Result<u64, RpcError> {
        let block = self.state_block(Some(&json!(U64::from(number))))?;
        let chain = self.chain.read().expect("chain lock");
        let account = self.rpc_state(&chain, &block)?.account(&sender)?;
        Ok(account.map_or(0, |account| account.nonce))
    }

    /// reth's search: the first block from 1 to the head after which `sender`'s nonce passed
    /// `nonce`, then the transaction from `sender` with that nonce in it.
    fn transaction_by_sender_and_nonce(&self, sender: Address, nonce: u64) -> Result<Value, RpcError> {
        let head = self.chain.read().expect("chain lock").head().number();
        if nonce >= self.nonce_at(sender, head)? {
            return Ok(Value::Null);
        }
        let (mut low, mut high, mut found) = (1u64, head, head);
        while low <= high {
            let mid = (low + high) / 2;
            if self.nonce_at(sender, mid)? > nonce {
                high = mid - 1;
                found = mid;
            } else {
                low = mid + 1;
            }
        }
        let block = self.state_block(Some(&json!(U64::from(found))))?;
        let index = (0..block.transactions.len())
            .find(|&index| block.senders[index] == sender && block.transactions[index].nonce() == nonce);
        Ok(index.map_or(Value::Null, |index| self.transaction_json(&block, index)))
    }
}
