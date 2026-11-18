//! Drives a synthetic chain into real nodes over the Engine API.

use std::{collections::BTreeSet, fs, io::Write, path::PathBuf, thread, time::Duration};

use alloy_consensus::Header;
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_common_consensus::BaseTxEnvelope;
use eyre::{WrapErr, bail, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Adapter, Blocks, ChainGenerator, GeneratorOptions, Log, Receipt,
    nodes::{NodeClient, NodeGenesis},
};

/// One transaction of the followed chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainTx {
    /// Transaction hash.
    pub hash: B256,
    /// Block number.
    pub block: u64,
    /// Index in the block.
    pub index: u64,
    /// EIP-2718 encoding.
    pub raw: Bytes,
    /// Sender (the `from` of a deposit).
    pub from: Address,
    /// EIP-2718 type.
    pub tx_type: u8,
    /// Receipt status.
    pub status: u8,
    /// Logs.
    pub logs: Vec<Log>,
}

/// One block of the followed chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainBlock {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Header timestamp.
    pub timestamp: u64,
    /// Indices into [`ChainIndex::txs`] of the block's transactions.
    pub txs: Vec<usize>,
}

/// What the request generator knows about the chain the nodes follow.
#[derive(Debug, Clone, Default)]
pub struct ChainIndex {
    /// Blocks, genesis first.
    pub blocks: Vec<ChainBlock>,
    /// Every transaction, in chain order.
    pub txs: Vec<ChainTx>,
    /// Addresses that emitted logs.
    pub log_addresses: BTreeSet<Address>,
    /// Log topics seen, capped.
    pub topics: Vec<B256>,
}

impl ChainIndex {
    /// Most distinct topics kept.
    pub const MAX_TOPICS: usize = 4096;

    /// The newest block.
    pub fn head(&self) -> &ChainBlock {
        self.blocks.last().expect("the index always holds the genesis block")
    }

    fn add_block(&mut self, header: &Header, hash: B256, txs: &[Bytes], receipts: &[Receipt]) {
        let mut indices = Vec::with_capacity(txs.len());
        for (index, (raw, receipt)) in txs.iter().zip(receipts).enumerate() {
            let from = BaseTxEnvelope::decode_2718_exact(raw.as_ref())
                .ok()
                .and_then(|tx| match &tx {
                    BaseTxEnvelope::Deposit(deposit) => Some(deposit.from),
                    tx => alloy_consensus::transaction::SignerRecoverable::recover_signer(tx).ok(),
                })
                .unwrap_or_default();
            for log in &receipt.logs {
                self.log_addresses.insert(log.address);
                for topic in &log.topics {
                    if self.topics.len() < Self::MAX_TOPICS && !self.topics.contains(topic) {
                        self.topics.push(*topic);
                    }
                }
            }
            indices.push(self.txs.len());
            self.txs.push(ChainTx {
                hash: keccak256(raw),
                block: header.number,
                index: index as u64,
                raw: raw.clone(),
                from,
                tx_type: receipt.tx_type,
                status: receipt.status,
                logs: receipt.logs.clone(),
            });
        }
        self.blocks.push(ChainBlock {
            number: header.number,
            hash,
            timestamp: header.timestamp,
            txs: indices,
        });
    }

    /// Decodes a transaction of the chain.
    pub fn decode(tx: &ChainTx) -> Option<BaseTxEnvelope> {
        BaseTxEnvelope::decode_2718_exact(tx.raw.as_ref()).ok()
    }
}

/// How far behind the head the safe and finalized blocks are kept.
#[derive(Debug, Clone, Copy)]
pub struct Lag {
    /// Blocks between safe and head.
    pub safe: u64,
    /// Blocks between finalized and head.
    pub finalized: u64,
}

/// A synthetic chain that every node follows, built on the reference adapter's responses.
#[derive(Debug)]
pub struct ChainFollower {
    /// The chain generator.
    pub chain: ChainGenerator,
    /// Reference adapter that computes every block.
    pub adapter: Adapter,
    /// Nodes under test.
    pub nodes: Vec<NodeClient>,
    /// What is known about the chain so far.
    pub index: ChainIndex,
    /// Safe and finalized lag.
    pub lag: Lag,
    /// File every submitted `engine_newPayloadV4` parameter list is appended to.
    pub payloads: Option<PathBuf>,
}

impl ChainFollower {
    /// Writes the genesis file of chain `seed` (Isthmus to Denim) to `path`, at block zero when
    /// `zero_number` is set (see [`NodeGenesis::json`]).
    pub fn write_genesis(
        seed: u64,
        options: GeneratorOptions,
        path: &std::path::Path,
        zero_number: bool,
    ) -> eyre::Result<()> {
        let chain = ChainGenerator::new(seed, options);
        let parent = &chain.ancestors()[0];
        // Nodes anchor the Denim subsecond schedule at the genesis block, assuming it sits on a
        // legacy whole-second slot; a genesis after Denim activation has a millisecond part no
        // genesis file can carry, so its chain cannot be followed.
        let denim = crate::Era::Denim.activation_timestamp();
        ensure!(
            parent.timestamp < denim,
            "chain {seed} starts after Denim activation; pick another seed"
        );
        let genesis = NodeGenesis::json(chain.state(), parent, zero_number);
        fs::write(path, serde_json::to_vec_pretty(&genesis)?)?;
        Ok(())
    }

    /// Starts following chain `seed` on nodes that were started from its genesis file. Every node
    /// must report the same genesis block; the chain is rebased onto it. The chain is a function
    /// of the seed, so after a restart the same blocks are built again; nodes answer `VALID` for
    /// blocks they already hold.
    pub fn start(
        seed: u64,
        options: GeneratorOptions,
        adapter: Adapter,
        nodes: Vec<NodeClient>,
        lag: Lag,
        payloads: Option<PathBuf>,
    ) -> eyre::Result<Self> {
        let mut chain = ChainGenerator::new(seed, options);
        let number = chain.ancestors()[0].number;
        let mut genesis: Option<(String, Value)> = None;
        for node in &nodes {
            let block =
                node.result("eth_getBlockByNumber", &json!([format!("{number:#x}"), false]))?;
            let hash = block.get("hash").cloned().unwrap_or_default();
            match &genesis {
                None => genesis = Some((node.name.clone(), block)),
                Some((name, first)) => ensure!(
                    first.get("hash") == Some(&hash),
                    "genesis block differs: {name} {} vs {} {hash}",
                    first["hash"],
                    node.name
                ),
            }
        }
        let (_, block) = genesis.ok_or_else(|| eyre!("no nodes"))?;
        let rpc: alloy_rpc_types_eth::Header<Header> =
            serde_json::from_value(block).wrap_err("decoding the genesis header")?;
        let header = rpc.inner;
        ensure!(header.hash_slow() == rpc.hash, "genesis header does not hash to its hash");
        chain.rebase(header.clone())?;
        let mut index = ChainIndex::default();
        index.add_block(&header, rpc.hash, &[], &[]);
        Ok(Self { chain, adapter, nodes, index, lag, payloads })
    }

    /// Builds the next block with the reference adapter, submits it to every node, and moves
    /// their heads to it. Fails when the adapter cannot build it or any node does not accept it.
    pub fn step(&mut self) -> eyre::Result<&ChainBlock> {
        let mut response = self.adapter.call(self.chain.request()).map_err(|e| eyre!("{e}"))?;
        if let Some(base_fee) = response.computed_base_fee_per_gas
            && base_fee != U256::from(self.chain.request().header.base_fee_per_gas)
        {
            self.chain.set_base_fee(
                u64::try_from(base_fee).wrap_err("computed base fee does not fit a header")?,
            );
            response = self.adapter.call(self.chain.request()).map_err(|e| eyre!("{e}"))?;
        }
        ensure!(
            response.ok && response.valid,
            "reference adapter could not build: {:?} {:?}",
            response.error,
            response.invalid_reason
        );
        let request = self.chain.request().clone();
        let results = response.header.clone().ok_or_else(|| eyre!("no header in response"))?;
        let header = Blocks::assemble(self.index.head().hash, &request.header, &results)?;
        let rejected: BTreeSet<usize> = response.rejected.iter().map(|r| r.index).collect();
        let txs: Vec<Bytes> = request
            .transactions
            .iter()
            .enumerate()
            .filter(|(i, _)| !rejected.contains(i))
            .map(|(_, tx)| tx.clone())
            .collect();
        let hash = results.hash;
        let (method, params) = Self::new_payload_params(&header, hash, &txs)?;
        if let Some(path) = &self.payloads {
            let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(file, "{}", json!({ "method": method, "params": params }))?;
        }
        let head = header.number;
        let forkchoice = json!([{
            "headBlockHash": hash,
            "safeBlockHash": self.ancestor_hash(head, self.lag.safe),
            "finalizedBlockHash": self.ancestor_hash(head, self.lag.finalized),
        }, null]);
        // Every node validates the block before any head moves, so a block one node rejects
        // leaves all heads on its parent.
        for node in &self.nodes {
            Self::new_payload(node, method, &params)
                .wrap_err_with(|| format!("{} did not accept block {head} {hash}", node.name))?;
        }
        for node in &self.nodes {
            Self::forkchoice(node, &forkchoice)
                .wrap_err_with(|| format!("{} did not move to block {head} {hash}", node.name))?;
        }
        self.chain.next(&response)?;
        self.index.add_block(&header, hash, &txs, &response.receipts);
        Ok(self.index.head())
    }

    fn ancestor_hash(&self, head: u64, lag: u64) -> B256 {
        let genesis = self.index.blocks[0].number;
        let number = head.saturating_sub(lag).max(genesis);
        if number == head {
            return B256::ZERO;
        }
        self.index.blocks[(number - genesis) as usize].hash
    }

    /// Submits a payload, retrying while the node answers `SYNCING` or `ACCEPTED`; fails unless
    /// it ends `VALID`.
    pub fn new_payload(node: &NodeClient, method: &str, params: &Value) -> eyre::Result<()> {
        for attempt in 0..50 {
            let status = node.engine(method, params)?;
            match status["status"].as_str() {
                Some("VALID") => return Ok(()),
                Some("SYNCING" | "ACCEPTED") if attempt < 49 => {
                    thread::sleep(Duration::from_millis(100))
                }
                _ => bail!("{method}: {status}"),
            }
        }
        unreachable!("the last attempt returns or fails")
    }

    /// Sends a forkchoice update (a zero safe or finalized hash stands for the head), retrying
    /// while the node answers `SYNCING` or `ACCEPTED`; fails unless it ends `VALID`.
    pub fn forkchoice(node: &NodeClient, forkchoice: &Value) -> eyre::Result<()> {
        let mut forkchoice = forkchoice.clone();
        // The head's own hash stands in for "safe/finalized = head" (lag 0).
        let head = forkchoice[0]["headBlockHash"].clone();
        for key in ["safeBlockHash", "finalizedBlockHash"] {
            if forkchoice[0][key] == json!(B256::ZERO) {
                forkchoice[0][key] = head.clone();
            }
        }
        for attempt in 0..50 {
            let updated = node.engine("engine_forkchoiceUpdatedV3", &forkchoice)?;
            match updated["payloadStatus"]["status"].as_str() {
                Some("VALID") => return Ok(()),
                Some("SYNCING" | "ACCEPTED") if attempt < 49 => {
                    thread::sleep(Duration::from_millis(100))
                }
                _ => bail!("engine_forkchoiceUpdatedV3: {updated}"),
            }
        }
        unreachable!("the last attempt returns or fails")
    }

    /// The Engine API method and parameters that submit a block. From Isthmus (headers with a
    /// requests hash): `engine_newPayloadV4` with the V3 payload plus the message passer storage
    /// root as `withdrawalsRoot`, no blob hashes, the parent beacon block root, and no execution
    /// requests. Before: `engine_newPayloadV3` with the V3 payload, no blob hashes, and the parent
    /// beacon block root.
    pub fn new_payload_params(
        header: &Header,
        hash: B256,
        txs: &[Bytes],
    ) -> eyre::Result<(&'static str, Value)> {
        let q = |v: u64| format!("{v:#x}");
        let mut payload = json!({
            "parentHash": header.parent_hash,
            "feeRecipient": header.beneficiary,
            "stateRoot": header.state_root,
            "receiptsRoot": header.receipts_root,
            "logsBloom": header.logs_bloom,
            "prevRandao": header.mix_hash,
            "blockNumber": q(header.number),
            "gasLimit": q(header.gas_limit),
            "gasUsed": q(header.gas_used),
            "timestamp": q(header.timestamp),
            "extraData": header.extra_data,
            "baseFeePerGas": q(header.base_fee_per_gas.unwrap_or_default()),
            "blockHash": hash,
            "transactions": txs,
            "withdrawals": [],
            "blobGasUsed": q(header.blob_gas_used.unwrap_or_default()),
            "excessBlobGas": q(header.excess_blob_gas.unwrap_or_default()),
        });
        let beacon =
            header.parent_beacon_block_root.ok_or_else(|| eyre!("no parent beacon root"))?;
        if header.requests_hash.is_none() {
            return Ok(("engine_newPayloadV3", json!([payload, [], beacon])));
        }
        payload["withdrawalsRoot"] =
            json!(header.withdrawals_root.ok_or_else(|| eyre!("no withdrawals root"))?);
        Ok(("engine_newPayloadV4", json!([payload, [], beacon, []])))
    }
}
