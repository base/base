//! JSON-RPC methods on the HTTP port (and the auth port): block, transaction and state reads.

use std::sync::Arc;

use alloy_consensus::transaction::Recovered;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, U64, U256};
use alloy_serde::JsonStorageKey;
use alloy_rpc_types_eth::TransactionInfo;
use base_common_consensus::{BaseTransactionInfo, DepositInfo};
use base_common_base_tx::NonceManagerSlots;
use serde_json::{Value, json};

use crate::{Node, RpcError, chain::StoredBlock};

/// JSON-RPC options, named and defaulted as base-reth's flags.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// `--rpc.eth-proof-window`: how many blocks below the head `eth_getProof` serves (reth's
    /// default 0: the head only).
    pub eth_proof_window: u64,
    /// `--http.api`: the namespaces served over HTTP, which `rpc_modules` lists.
    pub http_api: Vec<String>,
    /// `--rollup.sequencer` (or `-http`, `-ws`): where `eth_sendRawTransaction` and
    /// `base_transactionStatus` go.
    pub sequencer: Option<String>,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self { eth_proof_window: 0, http_api: ["eth", "net", "web3"].map(str::to_owned).to_vec(), sequencer: None }
    }
}

impl RpcConfig {
    /// reth's standard modules, which `--http.api all` selects.
    const ALL_MODULES: [&str; 14] = [
        "admin", "debug", "eth", "net", "trace", "txpool", "web3", "rpc", "reth", "ots", "flashbots", "miner",
        "mev", "testing",
    ];

    /// Sets `--http.api` as reth parses an `RpcModuleSelection`: `all` or `none` (any case) as
    /// the first entry, otherwise a comma-separated list of case-sensitive names.
    pub fn with_http_api(self, selection: &str) -> Self {
        if selection.is_empty() {
            return Self { http_api: Vec::new(), ..self };
        }
        let mut modules = selection.split(',').map(str::trim).peekable();
        let http_api = match modules.peek().map(|first| first.to_lowercase()).as_deref() {
            Some("all") => Self::ALL_MODULES.map(str::to_owned).to_vec(),
            Some("none") => Vec::new(),
            _ => {
                let mut selected: Vec<String> = Vec::new();
                for module in modules {
                    if !selected.iter().any(|known| known == module) {
                        selected.push(module.to_owned());
                    }
                }
                selected
            }
        };
        Self { http_api, ..self }
    }

    /// Whether HTTP serves `method`: its namespace is selected, or it is one base-reth always
    /// registers (`base_*`) or tiger's own fuzzing hook.
    pub fn serves(&self, method: &str) -> bool {
        let namespace = method.split_once('_').map_or(method, |(namespace, _)| namespace);
        matches!(namespace, "base" | "fuzz") || self.http_api.iter().any(|module| module == namespace)
    }

    /// reth's `MAX_ETH_PROOF_WINDOW`: 28 days of 2-second blocks.
    pub const MAX_ETH_PROOF_WINDOW: u64 = 28 * 24 * 60 * 60 / 2;

    /// Sets `--rpc.eth-proof-window`, rejecting values above reth's maximum as reth does.
    pub fn with_eth_proof_window(self, blocks: u64) -> Result<Self, String> {
        if blocks > Self::MAX_ETH_PROOF_WINDOW {
            return Err(format!(
                "--rpc.eth-proof-window {blocks}: value cannot be greater than {}",
                Self::MAX_ETH_PROOF_WINDOW
            ));
        }
        Ok(Self { eth_proof_window: blocks, ..self })
    }
}

mod blocks;
mod call;
mod envelope;
mod fees;
mod filters;
pub use filters::RpcFilters;
mod receipts;
mod sequencer;
mod shared;

fn param<T: serde::de::DeserializeOwned>(params: &Value, index: usize) -> Result<T, RpcError> {
    let value = params.get(index).cloned().unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|error| RpcError::invalid_params(format!("invalid params: {error}")))
}

impl Node {
    /// Answers one non-Engine JSON-RPC method.
    pub fn rpc(&self, method: &str, p: &Value) -> Result<Value, RpcError> {
        if let Some(result) = self.rpc_blocks(method, p).or_else(|| self.rpc_call(method, p)).or_else(|| self.rpc_filters(method, p)).or_else(|| self.rpc_fees(method, p)).or_else(|| self.rpc_sequencer(method, p)) {
            return result;
        }
        match method {
            "rpc_modules" => Ok(Value::Object(
                self.rpc_config.http_api.iter().map(|module| (module.clone(), json!("1.0"))).collect(),
            )),
            "eth_chainId" => Ok(json!(U64::from(self.spec.chain_id))),
            "net_version" => Ok(json!(self.spec.chain_id.to_string())),
            "web3_clientVersion" => Ok(json!(concat!("tiger/v", env!("CARGO_PKG_VERSION")))),
            "eth_syncing" => Ok(json!(false)),
            "eth_blockNumber" => {
                Ok(json!(U64::from(self.chain.read().expect("chain lock").head().number())))
            }
            "eth_getBlockByNumber" | "eth_getBlockByHash" => {
                let block = self.block_by_hash_or_tag(method.ends_with("ByHash"), p.get(0))?;
                let full: bool = param(p, 1)?;
                Ok(block.map_or(Value::Null, |block| self.block_json(&block, full)))
            }
            "eth_getTransactionCount" if param::<Option<U256>>(p, 2)?.is_some_and(|key| !key.is_zero()) => {
                let address: Address = param(p, 0)?;
                let key: U256 = param(p, 2)?;
                self.channel_nonce(address, key, p.get(1))
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
                let slot = U256::from_be_bytes(param::<JsonStorageKey>(p, 1)?.as_b256().0);
                let block = self.state_block(p.get(2))?;
                let chain = self.chain.read().expect("chain lock");
        let state = chain.state_after(&block).map_err(RpcError::internal)?;
                let value = state.storage(&address, &slot).map_err(|error| RpcError::internal(error.to_string()))?;
                Ok(json!(B256::from(value)))
            }
            "eth_getProof" => {
                let address: Address = param(p, 0)?;
                // Keys echo back in the form given: a 32-byte hash or a number.
                let keys: Vec<JsonStorageKey> = param(p, 1)?;
                // reth checks the window on the block's number before looking a numbered block up.
                let target = match p.get(2).map(|tag| serde_json::from_value::<BlockId>(tag.clone())) {
                    Some(Ok(BlockId::Number(BlockNumberOrTag::Number(number)))) => number,
                    Some(Ok(BlockId::Number(BlockNumberOrTag::Earliest))) => 0,
                    // The provider has no pending block.
                    Some(Ok(BlockId::Number(BlockNumberOrTag::Pending))) => {
                        return Err(RpcError::new(-32001, "block not found: pending"));
                    }
                    _ => self.state_block(p.get(2))?.number(),
                };
                let head = self.chain.read().expect("chain lock").head().number();
                if head.saturating_sub(target) > self.rpc_config.eth_proof_window {
                    return Err(RpcError::invalid_params(
                        "distance to target block exceeds maximum proof window".into(),
                    ));
                }
                let block = self.state_block(p.get(2))?;
                self.proof(&block, address, &keys)
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
        let id: Option<BlockId> = match tag {
            None => None,
            Some(tag) => serde_json::from_value(tag.clone()).map_err(|error| RpcError::invalid_params(error.to_string()))?,
        };
        Ok(match id {
            None => Some(chain.head()),
            Some(BlockId::Number(tag)) => match tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => Some(chain.head()),
                // reth's earliest block is number zero, whatever the genesis number.
                BlockNumberOrTag::Earliest => chain.canonical_block(0),
                BlockNumberOrTag::Safe => chain.block(&forkchoice.safe),
                BlockNumberOrTag::Finalized => chain.block(&forkchoice.finalized),
                BlockNumberOrTag::Number(number) => chain.canonical_block(number),
            },
            Some(BlockId::Hash(hash)) => chain
                .block(&hash.block_hash)
                .filter(|block| !hash.require_canonical.unwrap_or(false) || chain.is_canonical(&block.hash)),
        })
    }

    /// The block a `*ByHash` method (a bare hash) or a `*ByNumber` method (a number or tag)
    /// names, parsed as reth's parameter types do.
    pub fn block_by_hash_or_tag(&self, by_hash: bool, param: Option<&Value>) -> Result<Option<Arc<StoredBlock>>, RpcError> {
        let param = param.cloned().unwrap_or_default();
        let invalid = |error: serde_json::Error| RpcError::invalid_params(error.to_string());
        if by_hash {
            let hash: B256 = serde_json::from_value(param).map_err(invalid)?;
            return Ok(self.chain.read().expect("chain lock").block(&hash));
        }
        let tag: BlockNumberOrTag = serde_json::from_value(param).map_err(invalid)?;
        self.find_block(Some(&json!(tag)))
    }

    /// The block whose post-state a state read uses; an unknown block is an error.
    pub fn state_block(&self, tag: Option<&Value>) -> Result<Arc<StoredBlock>, RpcError> {
        self.find_block(tag)?.ok_or_else(|| RpcError::new(-32001, "block not found"))
    }

    /// base-reth's `ChannelNonceReader`: the Base transaction channel nonce of `address` under
    /// a nonzero `key`, read from the nonce manager's storage after Everest.
    fn channel_nonce(&self, address: Address, key: U256, tag: Option<&Value>) -> Result<Value, RpcError> {
        let block = self.everest_gate(tag)?;
        if key == U256::MAX {
            return Err(RpcError::invalid_params(
                "nonce_key NONCE_KEY_MAX selects the expiring-nonce channel which has no per-channel counter".into(),
            ));
        }
        let slot = NonceManagerSlots::nonce_slot(address, key).expect("the key is not the protocol key");
        let chain = self.chain.read().expect("chain lock");
        let state = chain.state_after(&block).map_err(RpcError::internal)?;
        let word = state.storage(&NonceManagerSlots::ADDRESS, &slot).map_err(RpcError::internal)?;
        Ok(json!(word & U256::from(u64::MAX)))
    }

    /// `eth_getProof` at the post-state of `block`.
    fn proof(&self, block: &StoredBlock, address: Address, keys: &[JsonStorageKey]) -> Result<Value, RpcError> {
        let chain = self.chain.read().expect("chain lock");
        let state = chain.state_after(block).map_err(RpcError::internal)?;
        let slots: Vec<U256> = keys.iter().map(|key| U256::from_be_bytes(key.as_b256().0)).collect();
        let proof = state.proof(address, &slots).map_err(RpcError::internal)?;
        let storage_proof: Vec<Value> = keys
            .iter()
            .zip(&proof.storage)
            .map(|(key, storage)| json!({ "key": key, "value": storage.value, "proof": storage.proof }))
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

#[cfg(test)]
pub(super) mod test_utils {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{Value, json};

    use crate::{Genesis, Node};

    /// A contract that reverts with the single byte `0xaa`.
    pub(crate) const REVERTER: &str = "0x00000000000000000000000000000000000000aa";

    /// A node on a fresh datadir whose genesis (block 0, every fork through Jovian active) holds
    /// [`REVERTER`] and a funded account `0x…01`.
    pub(crate) fn node() -> Node {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir()
            .join(format!("tiger-rpc-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        let genesis = json!({
            "config": {
                "chainId": 8453, "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0, "istanbulBlock": 0,
                "muirGlacierBlock": 0, "berlinBlock": 0, "londonBlock": 0, "arrowGlacierBlock": 0,
                "grayGlacierBlock": 0, "mergeNetsplitBlock": 0, "bedrockBlock": 0, "regolithTime": 0,
                "canyonTime": 0, "deltaTime": 0, "ecotoneTime": 0, "fjordTime": 0, "graniteTime": 0,
                "holoceneTime": 0, "isthmusTime": 0, "jovianTime": 0, "blockTime": 2,
                "terminalTotalDifficulty": 0, "terminalTotalDifficultyPassed": true,
                "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50, "eip1559DenominatorCanyon": 250 }
            },
            "nonce": "0x0", "timestamp": "0x0", "extraData": "0x000000000100000006", "gasLimit": "0x1c9c380",
            "difficulty": "0x0", "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "coinbase": "0x4200000000000000000000000000000000000011", "baseFeePerGas": "0x1",
            "excessBlobGas": "0x0", "blobGasUsed": "0x0",
            "alloc": {
                REVERTER: { "balance": "0x0", "code": "0x60aa60005360016000fd" },
                "0x0000000000000000000000000000000000000001": { "balance": "0xde0b6b3a7640000" }
            }
        });
        let genesis = Genesis::parse(&genesis.to_string()).expect("test genesis parses");
        Node::open(&dir, genesis).expect("test node opens")
    }

    /// The node's reply to `body`.
    pub(crate) fn reply(node: &Node, body: &str) -> Option<Value> {
        node.handle_body(body.as_bytes(), false)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{RpcConfig, test_utils::{node, reply}};

    #[test]
    fn http_api_selects_namespaces_as_reth() {
        let mut node = node();
        let modules = r#"{"jsonrpc":"2.0","id":1,"method":"rpc_modules","params":[]}"#;
        let raw_header = r#"{"jsonrpc":"2.0","id":1,"method":"debug_getRawHeader","params":["latest"]}"#;
        // reth's default selection serves neither `rpc_` nor `debug_`.
        assert_eq!(reply(&node, modules).unwrap()["error"]["code"], json!(-32601));
        assert_eq!(reply(&node, raw_header).unwrap()["error"]["code"], json!(-32601));
        node.rpc_config = RpcConfig::default().with_http_api("eth,net,web3,debug,admin,rpc");
        assert!(reply(&node, raw_header).unwrap()["result"].is_string());
        let listed = reply(&node, modules).unwrap()["result"].clone();
        assert_eq!(
            listed,
            json!({ "eth": "1.0", "net": "1.0", "web3": "1.0", "debug": "1.0", "admin": "1.0", "rpc": "1.0" })
        );
        assert_eq!(RpcConfig::default().with_http_api("ALL").http_api.len(), 14);
        assert!(RpcConfig::default().with_http_api("none").http_api.is_empty());
        assert!(RpcConfig::default().with_http_api("Eth").serves("base_transactionStatus"));
        assert!(!RpcConfig::default().with_http_api("Eth").serves("eth_chainId"), "names are case-sensitive");
    }
}
