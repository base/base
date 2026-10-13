use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::Path,
};

use alloy_consensus::{Header, Transaction, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B64, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use base_common_chains::ChainConfig;
use base_common_consensus::{
    BaseTxEnvelope, HoloceneExtraData, JovianExtraData, decode_2718_canonical,
};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_proof_mpt::{TrieNode, TrieProvider};
use clap::Args;
use eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{ExecutionWitness, OpenedState, RecordedRpc, Rpc};

/// Explicit storage access to capture as `ADDRESS:SLOT` (hexadecimal).
#[derive(Clone, Debug)]
pub struct StorageRequest {
    /// Account owning the storage trie.
    pub address: Address,
    /// Unhashed storage key.
    pub slot: B256,
}

impl std::str::FromStr for StorageRequest {
    type Err = eyre::Report;

    fn from_str(value: &str) -> Result<Self> {
        let (address, slot) =
            value.split_once(':').ok_or_else(|| eyre!("expected ADDRESS:SLOT"))?;
        Ok(Self {
            address: address.parse()?,
            slot: U256::from_str_radix(slot.trim_start_matches("0x"), 16)?.into(),
        })
    }
}

/// Selection filters intersect; selected transactions retain canonical block order.
#[derive(Clone, Debug, Args)]
pub struct Capture {
    /// Block number or tag. Prefer a recent safe block within the RPC proof window.
    #[arg(long, default_value = "safe")]
    pub block: String,
    /// Transaction indices, comma-separated. All requested indices must exist.
    #[arg(long, value_delimiter = ',')]
    pub indices: Vec<usize>,
    /// Transaction hashes, comma-separated. All requested hashes must exist in the block.
    #[arg(long, value_delimiter = ',')]
    pub transactions: Vec<B256>,
    /// Include only transactions sent to this address.
    #[arg(long)]
    pub to: Option<Address>,
    /// Maximum number of matching transactions.
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: u32,
    /// Capture parent-state account proofs for selected senders and recipients.
    #[arg(long)]
    pub proofs: bool,
    /// Additional account/storage proofs; repeat for multiple slots. Requires --proofs.
    #[arg(long, requires = "proofs")]
    pub storage: Vec<StorageRequest>,
}

/// One selected transaction and independently captured identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureTransaction {
    /// Original position in the source block, not in the selected subset.
    pub index: usize,
    /// Hash reported by RPC, checked against canonical encoding.
    pub hash: B256,
    /// Sender reported by RPC, checked against signature recovery.
    pub sender: Address,
    /// Canonically encoded EIP-2718 envelope, including deposit transactions.
    pub encoded: Bytes,
}

/// Portable component workload. Parent-state proofs are not per-transaction prestates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fixture {
    /// Schema version; unsupported versions fail rather than guessing defaults.
    pub version: u32,
    /// Captured chain identity (Base mainnet only).
    pub chain_id: u64,
    /// Source block hash, checked against the header.
    pub block_hash: B256,
    /// Source block's execution environment.
    pub header: Header,
    /// Parent header anchoring the captured state proofs.
    pub parent: Header,
    /// Selected transactions in block order.
    pub transactions: Vec<FixtureTransaction>,
    /// EIP-1186 account/storage witnesses at the parent block.
    pub proofs: Vec<EIP1186AccountProofResponse>,
    /// Optional transport-level mock for callers of [`Rpc`].
    pub rpc: RecordedRpc,
    /// Complete block execution witness, absent for transaction-selection fixtures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionWitness>,
}

impl Capture {
    /// Captures only read-only RPC data and validates it before returning.
    pub async fn capture<R: Rpc>(&self, rpc: &mut R) -> Result<Fixture> {
        ensure!(self.limit > 0, "limit must be positive");
        ensure!(self.proofs || self.storage.is_empty(), "storage capture requires proofs");
        let chain: String = serde_json::from_value(rpc.request("eth_chainId", json!([])).await?)?;
        ensure!(chain == "0x2105", "expected Base mainnet chain ID 8453, got {chain}");
        let block = if self.block.bytes().all(|b| b.is_ascii_digit()) && !self.block.is_empty() {
            format!("0x{:x}", self.block.parse::<u64>()?)
        } else {
            self.block.clone()
        };
        let value = rpc.request("eth_getBlockByNumber", json!([block, true])).await?;
        let header: Header = serde_json::from_value(value.clone())?;
        let block_hash = serde_json::from_value(value["hash"].clone())?;
        ensure!(header.hash_slow() == block_hash, "source header hash mismatch");
        let parent_value =
            rpc.request("eth_getBlockByHash", json!([header.parent_hash, false])).await?;
        let parent: Header = serde_json::from_value(parent_value)?;
        let txs =
            value["transactions"].as_array().ok_or_else(|| eyre!("missing full transactions"))?;
        for index in &self.indices {
            ensure!(*index < txs.len(), "transaction index {index} out of range");
        }
        for hash in &self.transactions {
            ensure!(
                txs.iter().any(|tx| tx["hash"] == json!(hash)),
                "transaction {hash} not in block"
            );
        }
        let mut transactions = Vec::new();
        let mut addresses = BTreeMap::<Address, BTreeSet<B256>>::new();
        for (index, value) in txs.iter().enumerate() {
            if !self.indices.is_empty() && !self.indices.contains(&index) {
                continue;
            }
            let hash = serde_json::from_value(value["hash"].clone())?;
            if !self.transactions.is_empty() && !self.transactions.contains(&hash) {
                continue;
            }
            let tx: BaseTxEnvelope = serde_json::from_value(value.clone())?;
            if self.to.is_some_and(|to| tx.to() != Some(to)) {
                continue;
            }
            let sender = serde_json::from_value(value["from"].clone())?;
            addresses.entry(sender).or_default();
            if let Some(to) = tx.to() {
                addresses.entry(to).or_default();
            }
            transactions.push(FixtureTransaction {
                index,
                hash,
                sender,
                encoded: tx.encoded_2718().into(),
            });
            if transactions.len() == self.limit as usize {
                break;
            }
        }
        ensure!(!transactions.is_empty(), "selection matched no transactions");
        for access in &self.storage {
            addresses.entry(access.address).or_default().insert(access.slot);
        }
        let mut proofs = Vec::new();
        if self.proofs {
            for (address, slots) in addresses {
                let value = rpc.request("eth_getProof", json!([
                    address, slots, {"blockHash": header.parent_hash, "requireCanonical": true}
                ])).await?;
                let proof: EIP1186AccountProofResponse = serde_json::from_value(value)?;
                ensure!(proof.address == address, "proof address mismatch");
                let returned: BTreeSet<B256> =
                    proof.storage_proof.iter().map(|s| s.key.as_b256()).collect();
                ensure!(returned == slots, "proof storage keys mismatch");
                proofs.push(proof);
            }
        }
        let fixture = Fixture {
            version: 1,
            chain_id: 8453,
            block_hash,
            header,
            parent,
            transactions,
            proofs,
            rpc: RecordedRpc::default(),
            execution: None,
        };
        fixture.validate()?;
        Ok(fixture)
    }
}

impl Fixture {
    /// Reads JSON or Zstandard-compressed JSON (`.zst`), without network access.
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input = BufReader::new(File::open(path)?);
        if path.extension().is_some_and(|ext| ext == "zst") {
            Ok(serde_json::from_reader(zstd::stream::read::Decoder::new(input)?)?)
        } else {
            Ok(serde_json::from_reader(input)?)
        }
    }

    /// Writes a new JSON fixture; `.zst` requests compression. Never overwrites existing files.
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let mut output = BufWriter::new(File::create_new(path)?);
        if path.extension().is_some_and(|ext| ext == "zst") {
            let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
            serde_json::to_writer(&mut encoder, self)?;
            encoder.finish()?.flush()?;
        } else {
            serde_json::to_writer_pretty(&mut output, self)?;
            output.flush()?;
        }
        Ok(())
    }

    /// Checks identities, canonical transaction bytes, recovered senders and proof values/roots.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported fixture version {}", self.version);
        ensure!(self.chain_id == 8453, "not a Base mainnet fixture");
        ensure!(self.header.hash_slow() == self.block_hash, "block hash mismatch");
        ensure!(self.parent.hash_slow() == self.header.parent_hash, "parent hash mismatch");
        ensure!(
            self.parent.number.checked_add(1) == Some(self.header.number),
            "nonconsecutive headers"
        );
        ensure!(!self.transactions.is_empty(), "empty transaction selection");
        ensure!(
            self.transactions.windows(2).all(|pair| pair[0].index < pair[1].index),
            "selection must be unique and in block order"
        );
        for tx in &self.transactions {
            let envelope: BaseTxEnvelope = decode_2718_canonical(&tx.encoded)?;
            ensure!(keccak256(&tx.encoded) == tx.hash, "transaction hash mismatch at {}", tx.index);
            ensure!(envelope.recover_signer()? == tx.sender, "sender mismatch at {}", tx.index);
            ensure!(
                envelope.chain_id().is_none_or(|id| id == self.chain_id),
                "wrong transaction chain ID"
            );
        }
        OpenedState::new(self)?;
        if let Some(witness) = &self.execution {
            witness.verify(self)?;
        }
        Ok(())
    }

    /// CL/EL boundary mock for transaction preparation, not an executable subset block.
    pub fn payload(&self) -> BasePayloadAttributes {
        let mut payload = BasePayloadAttributes {
            transactions: Some(self.transactions.iter().map(|tx| tx.encoded.clone()).collect()),
            no_tx_pool: Some(true),
            gas_limit: Some(self.header.gas_limit),
            ..Default::default()
        };
        payload.payload_attributes.timestamp = self.header.timestamp;
        payload.payload_attributes.prev_randao = self.header.mix_hash;
        payload.payload_attributes.suggested_fee_recipient = self.header.beneficiary;
        payload.payload_attributes.parent_beacon_block_root = self.header.parent_beacon_block_root;
        payload
    }

    /// Full-block CL/EL input using canonical fork parameters. Rejects partial selections
    /// before any state capture or execution, by checking the block's transaction commitment.
    pub fn execution_payload(&self) -> Result<BasePayloadAttributes> {
        ensure!(
            self.transactions.iter().enumerate().all(|(i, tx)| i == tx.index),
            "execution requires all block transactions in canonical order"
        );
        let transactions = self.payload().decoded_transactions().collect::<Result<Vec<_>, _>>()?;
        ensure!(
            alloy_consensus::proofs::calculate_transaction_root(&transactions)
                == self.header.transactions_root,
            "execution requires a complete block"
        );
        let config = ChainConfig::mainnet().rollup_config();
        let mut payload = self.payload();
        if config.is_canyon_active(self.header.timestamp) {
            payload.payload_attributes.withdrawals = Some(Vec::new());
        }
        let params = if config.is_jovian_active(self.header.timestamp) {
            let (elasticity, denominator, minimum) =
                JovianExtraData::decode(&self.header.extra_data)?;
            payload.min_base_fee = Some(minimum);
            Some((elasticity, denominator))
        } else if config.is_holocene_active(self.header.timestamp) {
            Some(HoloceneExtraData::decode(&self.header.extra_data)?)
        } else {
            None
        };
        if let Some((elasticity, denominator)) = params {
            let mut encoded = [0; 8];
            encoded[..4].copy_from_slice(&denominator.to_be_bytes());
            encoded[4..].copy_from_slice(&elasticity.to_be_bytes());
            payload.eip_1559_params = Some(B64::from(encoded));
        }
        Ok(payload)
    }

    /// Content-addressed state substitute; constructing it performs all preimage hashing once.
    pub fn provider(&self) -> WitnessProvider {
        WitnessProvider {
            nodes: self
                .proofs
                .iter()
                .flat_map(|proof| {
                    proof
                        .account_proof
                        .iter()
                        .chain(proof.storage_proof.iter().flat_map(|s| &s.proof))
                })
                .map(|bytes| (keccak256(bytes), bytes.clone()))
                .collect(),
        }
    }
}

/// Offline state storage. Missing nodes are errors, never zero/default state.
#[derive(Clone, Debug, Default)]
pub struct WitnessProvider {
    /// Raw, content-addressed RLP nodes. Use [`Fixture::provider`] to populate this map.
    pub nodes: BTreeMap<B256, Bytes>,
}

impl TrieProvider for WitnessProvider {
    type Error = eyre::Report;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode> {
        let bytes = self.nodes.get(&key).ok_or_else(|| eyre!("missing witness node {key}"))?;
        let mut input = bytes.as_ref();
        let node = TrieNode::decode(&mut input)?;
        ensure!(input.is_empty(), "trailing bytes in witness node");
        Ok(node)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Sealable;
    use alloy_primitives::TxKind;
    use base_common_consensus::TxDeposit;

    use super::*;
    use crate::{BenchmarkReport, RpcExchange};

    #[test]
    fn mainnet_replay_checks_values_and_rejects_incomplete_or_corrupt_inputs() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../fixtures/mainnet.json")).unwrap();
        fixture.validate().unwrap();
        let report = BenchmarkReport::run(&fixture, 3, 1).unwrap();
        assert_eq!(report.transactions, 3);
        assert_eq!(report.accounts, 6);
        assert_eq!(report.storage_slots, 1);
        let mut state = OpenedState::blinded(&fixture);
        assert!(state.read(&fixture, &WitnessProvider::default()).is_err());

        let mut corrupt = fixture.clone();
        corrupt.transactions[1].sender = Address::ZERO;
        assert!(corrupt.validate().is_err());
        let mut corrupt = fixture.clone();
        corrupt.proofs[0].storage_proof[0].value += U256::ONE;
        assert!(corrupt.validate().is_err());
        let mut corrupt = fixture.clone();
        corrupt.proofs[0].balance += U256::ONE;
        assert!(corrupt.validate().is_err());
        let mut corrupt = fixture.clone();
        corrupt.parent.state_root = B256::ZERO;
        assert!(corrupt.validate().is_err());
        let mut corrupt = fixture.clone();
        corrupt.transactions.reverse();
        assert!(corrupt.validate().is_err());
        let mut corrupt = fixture;
        corrupt.version += 1;
        assert!(corrupt.validate().is_err());
    }

    #[tokio::test]
    async fn capture_selection_preserves_order_and_checks_filters_offline() {
        let parent = Header { number: 40, ..Default::default() };
        let header = Header { number: 41, parent_hash: parent.hash_slow(), ..Default::default() };
        let target = Address::repeat_byte(8);
        let transactions: Vec<_> = (0..4u8)
            .map(|i| {
                let tx = BaseTxEnvelope::Deposit(
                    TxDeposit {
                        from: Address::repeat_byte(i + 1),
                        to: TxKind::Call(if i == 2 { Address::repeat_byte(9) } else { target }),
                        ..Default::default()
                    }
                    .seal_slow(),
                );
                let mut value = serde_json::to_value(&tx).unwrap();
                value["hash"] = json!(keccak256(tx.encoded_2718()));
                value
            })
            .collect();
        let wanted: B256 = serde_json::from_value(transactions[3]["hash"].clone()).unwrap();
        let mut block = serde_json::to_value(&header).unwrap();
        block["hash"] = json!(header.hash_slow());
        block["transactions"] = json!(transactions);
        let mut rpc = RecordedRpc {
            exchanges: vec![
                RpcExchange {
                    method: "eth_chainId".into(),
                    params: json!([]),
                    result: json!("0x2105"),
                },
                RpcExchange {
                    method: "eth_getBlockByNumber".into(),
                    params: json!(["0x29", true]),
                    result: block,
                },
                RpcExchange {
                    method: "eth_getBlockByHash".into(),
                    params: json!([header.parent_hash, false]),
                    result: json!(parent),
                },
            ],
        };
        let mut selection = Capture {
            block: "41".into(),
            indices: vec![3, 0, 2],
            transactions: Vec::new(),
            to: Some(target),
            limit: 10,
            proofs: false,
            storage: Vec::new(),
        };
        let captured = selection.capture(&mut rpc).await.unwrap();
        assert_eq!(captured.transactions.iter().map(|tx| tx.index).collect::<Vec<_>>(), vec![0, 3]);
        selection.limit = 1;
        assert_eq!(selection.capture(&mut rpc).await.unwrap().transactions[0].index, 0);
        selection.transactions = vec![wanted];
        assert_eq!(selection.capture(&mut rpc).await.unwrap().transactions[0].hash, wanted);
        selection.indices = vec![4];
        assert!(selection.capture(&mut rpc).await.is_err());
        selection.indices.clear();
        selection.transactions = vec![B256::ZERO];
        assert!(selection.capture(&mut rpc).await.is_err());
        selection.transactions.clear();
        selection.to = Some(Address::ZERO);
        assert!(selection.capture(&mut rpc).await.is_err());
    }
}
