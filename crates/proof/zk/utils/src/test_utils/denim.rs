//! Hermetic Denim range-proof fixture shared by native and packaged backends.
//!
//! The fixture models an agreed block at `#1@12.000` and six children at
//! `14.000/14.200/14.400/14.600/14.800/15.000`. It builds calldata channels and only the trie,
//! code, header, and boot preimages consumed by the proof. To regenerate and verify the vectors, run:
//! `cargo test -p base-proof-client --test denim_native -- --nocapture`.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use alloy_consensus::{
    Header, Sealable, SignableTransaction, TxEnvelope, TxLegacy, transaction::SignerRecoverable,
};
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, b256, bytes, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope, Predeploys};
use base_common_evm::{BaseEvmFactory, BaseTime};
use base_common_genesis::{
    BaseUpgradeConfig, ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig,
};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_comp::{ChannelOut, ZlibCompressor};
use base_proof::{
    BootInfo, INTERMEDIATE_BLOCK_INTERVAL_KEY, L1_CONFIG_KEY, L1_HEAD_KEY, L1_HEAD_NUMBER_KEY,
    L2_CHAIN_ID_KEY, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY, L2_OUTPUT_ROOT_KEY,
    L2_ROLLUP_CONFIG_KEY, L2_SCHEDULE_BLOCK_NUMBER_KEY, ScheduleId,
};
use base_proof_executor::{StatelessL2Builder, TrieDBProvider};
use base_proof_mpt::{NoopTrieHinter, TrieNode, TrieProvider, ordered_trie_with_encoder};
use base_proof_preimage::PreimageKey;
use base_protocol::{
    BaseTimeUpdateTx, DERIVATION_VERSION_0, L1BlockInfoTx, OutputRoot, SingleBatch,
};
use sha2::{Digest, Sha256};

use crate::{
    boot::BootInfoStruct,
    witness::{DefaultWitnessData, preimage_store::PreimageStore},
};

/// Synthetic L2 chain ID used by the fixture.
pub const DENIM_CHAIN_ID: u64 = 999_999_999;
const L1_CHAIN_ID: u64 = 999_999_998;
/// Final claimed L2 block in the fixture.
pub const CLAIM_BLOCK: u64 = 7;
/// Canonical per-chain config hash committed by the fixture.
pub const DENIM_CONFIG_HASH: B256 =
    b256!("80cc3f230d72195dde768904e4c6860232383ab0cd6786f12072ae7870aeae1a");
/// SHA-256 commitment to the sorted canonical fixture preimages.
pub const DENIM_FIXTURE_CONTENT_HASH: B256 =
    b256!("c0e191d340440075b3743ffe091a53cbdc5277c19e5d2ff596b5139d3061b83c");
/// Denim activation timestamp in the fixture.
pub const DENIM_TIMESTAMP: u64 = 14;
const INITIAL_GAS_LIMIT: u64 = 300_000_000;
const DENIM_GAS_LIMIT: u64 = 30_000_000;

#[derive(Debug, Clone)]
struct MemoryTrieDBProvider {
    trie_nodes: Arc<RwLock<BTreeMap<B256, Bytes>>>,
    bytecodes: BTreeMap<B256, Bytes>,
}

impl MemoryTrieDBProvider {
    fn capture(&self, node: &TrieNode) {
        match node {
            TrieNode::Extension { node, .. } => self.capture(node),
            TrieNode::Branch { stack } => {
                for node in stack {
                    self.capture(node);
                }
            }
            TrieNode::Empty | TrieNode::Blinded { .. } | TrieNode::Leaf { .. } => {}
        }
        if !matches!(node, TrieNode::Empty | TrieNode::Blinded { .. }) {
            let encoded = rlp(node);
            self.trie_nodes.write().unwrap().insert(keccak256(&encoded), encoded);
        }
    }
}

impl TrieProvider for MemoryTrieDBProvider {
    type Error = String;

    fn trie_node_by_hash(&self, hash: B256) -> Result<TrieNode, Self::Error> {
        let bytes = self
            .trie_nodes
            .read()
            .unwrap()
            .get(&hash)
            .cloned()
            .ok_or_else(|| format!("missing trie node {hash}"))?;
        TrieNode::decode(&mut bytes.as_ref()).map_err(|error| error.to_string())
    }
}

impl TrieDBProvider for MemoryTrieDBProvider {
    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes, Self::Error> {
        self.bytecodes.get(&hash).cloned().ok_or_else(|| format!("missing bytecode {hash}"))
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
        Err(format!("missing header {hash}"))
    }
}

/// Frozen commitments and `BaseTime` artifacts for one derived fixture block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedDenimBlock {
    /// L2 block number.
    pub number: u64,
    /// Whole-second L2 timestamp.
    pub timestamp: u64,
    /// Millisecond timestamp component committed by `BaseTime`.
    pub timestamp_millis_part: u16,
    /// Canonical block hash.
    pub block_hash: B256,
    /// Encoded `BaseTime` deposit transaction.
    pub base_time_transaction: Bytes,
    /// Encoded successful `BaseTime` receipt.
    pub base_time_receipt: Bytes,
    /// Post-execution state root.
    pub state_root: B256,
    /// Post-execution output root.
    pub output_root: B256,
}

/// Deterministic recorded-preimage fixture spanning Denim activation and same-second blocks.
#[derive(Debug, Clone)]
pub struct DenimFixture {
    /// Exact preimages consumed by proof execution.
    pub store: PreimageStore,
    /// Frozen per-block expected vectors.
    pub expected: Vec<ExpectedDenimBlock>,
    /// Pinned schedule commitment.
    pub schedule_id: B256,
    config: RollupConfig,
    agreed_block_number: u64,
    interval: u64,
}

impl DenimFixture {
    /// Builds the canonical fixture from safe block 1 to claimed block 7.
    pub fn new() -> Self {
        Self::with_options(None, None, 1, false)
    }

    /// Builds a fixture variant for native negative and restart coverage.
    pub fn with_options(
        agreed_index: Option<usize>,
        schedule_block: Option<u64>,
        interval: u64,
        malformed_batch: bool,
    ) -> Self {
        let l1_genesis = Header {
            number: 0,
            timestamp: 10,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1),
            transactions_root: EMPTY_ROOT_HASH,
            receipts_root: EMPTY_ROOT_HASH,
            ..Default::default()
        };
        let l1_genesis_hash = l1_genesis.hash_slow();
        let l2_genesis = Header {
            number: 0,
            timestamp: 10,
            gas_limit: INITIAL_GAS_LIMIT,
            base_fee_per_gas: Some(1_000_000),
            transactions_root: EMPTY_ROOT_HASH,
            receipts_root: EMPTY_ROOT_HASH,
            ..Default::default()
        };
        let l2_genesis_hash = l2_genesis.hash_slow();

        let mut config = RollupConfig {
            genesis: ChainGenesis {
                l1: (0, l1_genesis_hash).into(),
                l2: (0, l2_genesis_hash).into(),
                l2_time: 10,
                system_config: None,
            },
            block_time: 2,
            max_sequencer_drift: 600,
            seq_window_size: 4,
            channel_timeout: 10,
            l1_chain_id: L1_CHAIN_ID,
            l2_chain_id: DENIM_CHAIN_ID.into(),
            batch_inbox_address: Address::repeat_byte(0x22),
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(DENIM_TIMESTAMP), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };

        let batcher = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap();
        let system_config = SystemConfig {
            batcher_address: batcher.address(),
            gas_limit: INITIAL_GAS_LIMIT,
            ..Default::default()
        };
        config.genesis.system_config = Some(system_config);

        let l1_config = ChainConfig::default();
        let (initial_state_root, initial_provider) = initial_state();
        let safe_transaction =
            l1_info_transaction(&config, &l1_config, &system_config, 0, &l1_genesis, 10, 12);
        let safe_transactions = vec![safe_transaction];
        let (safe_transactions_root, safe_transaction_nodes) = ordered_trie(&safe_transactions);
        let safe_header = Header {
            parent_hash: l2_genesis_hash,
            state_root: initial_state_root,
            transactions_root: safe_transactions_root,
            receipts_root: EMPTY_ROOT_HASH,
            number: 1,
            timestamp: 12,
            gas_limit: INITIAL_GAS_LIMIT,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        }
        .seal_slow();
        let safe_hash = safe_header.hash();
        let safe_output =
            OutputRoot::from_parts(safe_header.state_root, EMPTY_ROOT_HASH, safe_hash);

        let mut store = PreimageStore::default();
        insert_keccak(&mut store, vec![alloy_rlp::EMPTY_STRING_CODE]);
        insert_header(&mut store, &l1_genesis);
        insert_header(&mut store, &l2_genesis);
        insert_header(&mut store, safe_header.inner());
        insert_nodes(&mut store, safe_transaction_nodes);
        insert_provider(&mut store, &initial_provider);
        insert_keccak(&mut store, safe_output.encode().to_vec());

        let mut builder = StatelessL2Builder::new(
            &config,
            BaseEvmFactory::default(),
            initial_provider.clone(),
            NoopTrieHinter,
            safe_header,
        );
        let mut expected = Vec::new();
        let schedule =
            [(2, 14, 0), (3, 14, 200), (4, 14, 400), (5, 14, 600), (6, 14, 800), (7, 15, 0)];
        for (number, timestamp, timestamp_millis_part) in schedule {
            let attributes = payload_attributes(
                &config,
                &l1_config,
                &system_config,
                &l1_genesis,
                number,
                timestamp,
                timestamp_millis_part,
            );
            let transactions = attributes.transactions.clone().unwrap();
            let outcome = builder.build_block(attributes).unwrap();
            let output_root = builder.compute_output_root().unwrap();
            let trie_db = builder.trie_db();
            initial_provider.capture(trie_db.root());
            for storage_root in trie_db.storage_roots().values() {
                initial_provider.capture(storage_root);
            }

            insert_provider(&mut store, &initial_provider);
            insert_header(&mut store, outcome.header.inner());
            let (transactions_root, transaction_nodes) = ordered_trie(&transactions);
            assert_eq!(transactions_root, outcome.header.transactions_root);
            insert_nodes(&mut store, transaction_nodes);

            expected.push(ExpectedDenimBlock {
                number,
                timestamp,
                timestamp_millis_part,
                block_hash: outcome.header.hash(),
                base_time_transaction: transactions[1].clone(),
                base_time_receipt: encode_receipt(&outcome.execution_result.receipts[1]),
                state_root: outcome.header.state_root,
                output_root,
            });
        }
        assert_eq!(expected, recorded_vectors());

        let batches = canonical_batches(l1_genesis_hash, safe_hash, &expected);
        let channel_data = if malformed_batch {
            Bytes::from_static(&[DERIVATION_VERSION_0, 0xff, 0xff])
        } else {
            encode_channel(&config, &batches)
        };
        let l1_batch_transaction =
            batch_transaction(config.batch_inbox_address, channel_data, &batcher);
        assert_eq!(l1_batch_transaction.recover_signer().unwrap(), system_config.batcher_address);
        let l1_transactions = vec![encode_l1_transaction(&l1_batch_transaction)];
        let (l1_transactions_root, l1_transaction_nodes) = ordered_trie(&l1_transactions);
        let l1_head = Header {
            parent_hash: l1_genesis_hash,
            number: 1,
            timestamp: 11,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1),
            transactions_root: l1_transactions_root,
            receipts_root: EMPTY_ROOT_HASH,
            ..Default::default()
        };
        insert_header(&mut store, &l1_head);
        insert_nodes(&mut store, l1_transaction_nodes);

        let agreed_output = agreed_index
            .map(|index| {
                let block = &expected[index];
                let output =
                    OutputRoot::from_parts(block.state_root, EMPTY_ROOT_HASH, block.block_hash);
                assert_eq!(output.hash(), block.output_root);
                output
            })
            .unwrap_or(safe_output);
        insert_keccak(&mut store, agreed_output.encode().to_vec());

        let claim = expected.last().unwrap().output_root;
        insert_local(&mut store, L1_HEAD_KEY, l1_head.hash_slow().to_vec());
        insert_local(&mut store, L2_OUTPUT_ROOT_KEY, agreed_output.hash().to_vec());
        insert_local(&mut store, L2_CLAIM_KEY, claim.to_vec());
        insert_local(&mut store, L2_CLAIM_BLOCK_NUMBER_KEY, CLAIM_BLOCK.to_be_bytes().to_vec());
        insert_local(&mut store, L2_CHAIN_ID_KEY, DENIM_CHAIN_ID.to_be_bytes().to_vec());
        insert_local(&mut store, L2_ROLLUP_CONFIG_KEY, serde_json::to_vec(&config).unwrap());
        insert_local(&mut store, L1_CONFIG_KEY, serde_json::to_vec(&l1_config).unwrap());
        insert_local(&mut store, INTERMEDIATE_BLOCK_INTERVAL_KEY, interval.to_be_bytes().to_vec());
        insert_local(&mut store, L1_HEAD_NUMBER_KEY, 1u64.to_be_bytes().to_vec());
        if let Some(schedule_block) = schedule_block {
            insert_local(
                &mut store,
                L2_SCHEDULE_BLOCK_NUMBER_KEY,
                schedule_block.to_be_bytes().to_vec(),
            );
        }

        let mut pinned = config.clone();
        let schedule_timestamp = pinned.l2_block_timestamp(schedule_block.unwrap_or(CLAIM_BLOCK));
        let schedule_id = ScheduleId::pin(&mut pinned, schedule_timestamp);
        let mut per_chain = base_proof_primitives::PerChainConfig::from_rollup_config(&pinned)
            .expect("fixture rollup config has a system config");
        per_chain.force_defaults();
        assert_eq!(per_chain.hash(), DENIM_CONFIG_HASH);
        let agreed_block_number = agreed_index.map(|index| expected[index].number).unwrap_or(1);
        Self { store, expected, schedule_id, config, agreed_block_number, interval }
    }

    /// Hashes the sorted preimage keys and values to identify fixture content across backends.
    pub fn content_hash(&self) -> B256 {
        let mut preimages = self.store.preimage_map.iter().collect::<Vec<_>>();
        preimages.sort_unstable_by_key(|(key, _)| **key);

        let mut hasher = Sha256::new();
        for (key, value) in preimages {
            hasher.update([key.key_type() as u8]);
            hasher.update(key.key_value().to_be_bytes::<32>());
            hasher.update(u64::try_from(value.len()).unwrap().to_be_bytes());
            hasher.update(value);
        }
        B256::from(<[u8; 32]>::from(hasher.finalize()))
    }

    /// Packages the fixture preimages through the range program's standard witness type.
    pub fn witness(&self) -> DefaultWitnessData {
        DefaultWitnessData { preimage_store: self.store.clone(), blob_data: Default::default() }
    }

    /// Replaces the claimed output root for backend rejection tests.
    pub fn with_claimed_output_root(mut self, root: B256) -> Self {
        insert_local(&mut self.store, L2_CLAIM_KEY, root.to_vec());
        self
    }

    /// Replaces the schedule block for backend rejection tests.
    pub fn with_schedule_block(mut self, block: u64) -> Self {
        insert_local(&mut self.store, L2_SCHEDULE_BLOCK_NUMBER_KEY, block.to_be_bytes().to_vec());
        let mut pinned = self.config.clone();
        self.schedule_id = ScheduleId::pin(&mut pinned, self.config.l2_block_timestamp(block));
        self
    }

    /// Constructs the exact public values expected from the SP1 range program.
    pub async fn expected_public_values(&self) -> BootInfoStruct {
        let boot_info = BootInfo::load(&self.store).await.unwrap();
        let roots = self
            .expected
            .iter()
            .filter(|block| block.number > self.agreed_block_number)
            .enumerate()
            .filter(|(index, _)| (index + 1) % self.interval.max(1) as usize == 0)
            .map(|(_, block)| block.output_root)
            .collect();
        BootInfoStruct::new(boot_info, self.agreed_block_number, CLAIM_BLOCK, roots)
    }
}

impl Default for DenimFixture {
    fn default() -> Self {
        Self::new()
    }
}

fn canonical_batches(
    epoch_hash: B256,
    safe_hash: B256,
    expected: &[ExpectedDenimBlock],
) -> Vec<SingleBatch> {
    [14, 14, 14, 14, 14, 15]
        .into_iter()
        .zip(std::iter::once(safe_hash).chain(expected.iter().map(|block| block.block_hash)))
        .map(|(timestamp, parent_hash)| SingleBatch {
            parent_hash,
            epoch_num: 0,
            epoch_hash,
            timestamp,
            transactions: Vec::new(),
        })
        .collect()
}

fn recorded_vectors() -> Vec<ExpectedDenimBlock> {
    vec![
        ExpectedDenimBlock {
            number: 2,
            timestamp: 14,
            timestamp_millis_part: 0,
            block_hash: b256!("92578f553757af00628c2f0cb05531a91780313763d9ee5f139a9576732e048b"),
            base_time_transaction: bytes!(
                "7ef877a0002c6109309955fa0a804192573fbb620035638bfc135b9190bc5f3dd964498a94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000308080830f424080a486bdf3940000000000000000000000000000000000000000000000000000000000000000"
            ),
            base_time_receipt: bytes!(
                "7ef9010a018408f1425db9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0"
            ),
            state_root: b256!("ba6a62600c29055d63c307659c08d3eb76c25ddeedb4b661fb1d3388a76229f2"),
            output_root: b256!("16e2449ea2bf31d8c59cbf54001826430f5056da6f0daebb1c4da973a3e8b072"),
        },
        ExpectedDenimBlock {
            number: 3,
            timestamp: 14,
            timestamp_millis_part: 200,
            block_hash: b256!("bcc827a866646ec3017c98233d73a24bac0a9246b3590cf8eec04aefe2b22606"),
            base_time_transaction: bytes!(
                "7ef877a01ad4ef655c527f07678b9b8a465b2fca1ebbfc4a7d3dec72485f547f49fce2a894deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000308080830f424080a486bdf39400000000000000000000000000000000000000000000000000000000000000c8"
            ),
            base_time_receipt: bytes!(
                "7ef9010a018408f19025b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0"
            ),
            state_root: b256!("7306fab99637e9de714d44a513eb4b3af2202566eca94cedfe700dd484704591"),
            output_root: b256!("0ec4cc78c04c75f60b4b42532a692209365d0509404ef9b5ace09e4a0018d44d"),
        },
        ExpectedDenimBlock {
            number: 4,
            timestamp: 14,
            timestamp_millis_part: 400,
            block_hash: b256!("b99d542d41154c574524d64e2edd8c638b03b69a0ffe5ebfaca2827c1258105a"),
            base_time_transaction: bytes!(
                "7ef877a0ea2c84bc64dc6b971c5eb945c1fa8ebfd4b7ef4bfa161b973a267f8e9b811c7094deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000308080830f424080a486bdf3940000000000000000000000000000000000000000000000000000000000000190"
            ),
            base_time_receipt: bytes!(
                "7ef9010a018408f14d65b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0"
            ),
            state_root: b256!("212d35fadcda8b28ae7ca2be0ece495179d2a6ef29f989becfc454408b2f1849"),
            output_root: b256!("5d6f9a01880409169b7929fa392074c1c56928e2afdebab30f46673350f4098f"),
        },
        ExpectedDenimBlock {
            number: 5,
            timestamp: 14,
            timestamp_millis_part: 600,
            block_hash: b256!("53ab8f670ea2a03d389ef7d0285294482084f7e4760d042f33eaa0f5877aca66"),
            base_time_transaction: bytes!(
                "7ef877a00be585d024fa4b6ffc196ed358916dbd4969df14fa53f8aae2d05134b8aee70f94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000308080830f424080a486bdf3940000000000000000000000000000000000000000000000000000000000000258"
            ),
            base_time_receipt: bytes!(
                "7ef9010a018408f14d65b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0"
            ),
            state_root: b256!("fd510779f10c8d5cee46e036ecd37edee3731c8cb571c040d58bfadae190d985"),
            output_root: b256!("72113e89c2c0a7d4189f51846385de4786a86a06945213b2c9849b11b9323eb5"),
        },
        ExpectedDenimBlock {
            number: 6,
            timestamp: 14,
            timestamp_millis_part: 800,
            block_hash: b256!("4daa8426e7f0c380bd165f0c1cce6b64433a3da2f61e0e51cd0c49bd14825928"),
            base_time_transaction: bytes!(
                "7ef877a0030b4b361f8d483df43821d49b773fbe9743fc895c2ed9e62e7d0c9087c1486894deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000308080830f424080a486bdf3940000000000000000000000000000000000000000000000000000000000000320"
            ),
            base_time_receipt: bytes!(
                "7ef9010a018408f14d65b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0"
            ),
            state_root: b256!("945002226a61039964b2846bdaf18d9557ac543ab44c6f2d6fc763a36e27c411"),
            output_root: b256!("095977aac93b01ba1202a364fec5c8bf3df861031e044a1d03dddf821faf66e9"),
        },
        ExpectedDenimBlock {
            number: 7,
            timestamp: 15,
            timestamp_millis_part: 0,
            block_hash: b256!("fe397f7acdc9a9fe7c847e435ce31570319b2e358b92f43d75ad9b17e29c1b7b"),
            base_time_transaction: bytes!(
                "7ef877a02d7c93d7a1a599ef29848393a32ed836fdcb64441c082a4489ee96becef46e2694deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000308080830f424080a486bdf3940000000000000000000000000000000000000000000000000000000000000000"
            ),
            base_time_receipt: bytes!(
                "7ef9010a018408f13a8db9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0"
            ),
            state_root: b256!("f45693a95937020d3cefeac29b15afd840246d1b4321e737b615885ab32b37fe"),
            output_root: b256!("9d6bf097a8e02f3d2c3c37665953ebfc8820208cbfc226b66d490962aa0a455b"),
        },
    ]
}

fn encode_channel(config: &RollupConfig, batches: &[SingleBatch]) -> Bytes {
    let mut channel = ChannelOut::new([0x44; 16], Arc::new(config.clone()), ZlibCompressor::new());
    for batch in batches {
        channel.add_single_batch(batch.clone()).unwrap();
    }
    let frames = channel.into_frames(120_000).unwrap();
    let mut encoded = vec![DERIVATION_VERSION_0];
    for frame in frames {
        encoded.extend_from_slice(&frame.encode());
    }
    encoded.into()
}

fn batch_transaction(to: Address, input: Bytes, signer: &PrivateKeySigner) -> TxEnvelope {
    let transaction =
        TxLegacy { to: TxKind::Call(to), gas_limit: 1_000_000, input, ..Default::default() };
    let signature = signer.sign_hash_sync(&transaction.signature_hash()).unwrap();
    TxEnvelope::Legacy(transaction.into_signed(signature))
}

fn l1_info_transaction(
    config: &RollupConfig,
    l1_config: &ChainConfig,
    system_config: &SystemConfig,
    sequence_number: u64,
    l1_header: &Header,
    parent_timestamp: u64,
    timestamp: u64,
) -> Bytes {
    let (_, transaction) = L1BlockInfoTx::try_new_with_deposit_tx(
        config,
        l1_config,
        system_config,
        sequence_number,
        l1_header,
        parent_timestamp,
        timestamp,
    )
    .unwrap();
    encode_base_transaction(&transaction.into())
}

fn payload_attributes(
    config: &RollupConfig,
    l1_config: &ChainConfig,
    initial_system_config: &SystemConfig,
    l1_header: &Header,
    number: u64,
    timestamp: u64,
    timestamp_millis_part: u16,
) -> BasePayloadAttributes {
    let mut system_config = *initial_system_config;
    system_config.gas_limit = DENIM_GAS_LIMIT;
    let l1_info = l1_info_transaction(
        config,
        l1_config,
        &system_config,
        number - 1,
        l1_header,
        if number == 2 { 12 } else { config.l2_block_timestamp(number - 1) },
        timestamp,
    );
    let base_time = encode_base_transaction(
        &BaseTimeUpdateTx::new(timestamp_millis_part).unwrap().into_deposit_tx(number).into(),
    );
    let mut attributes = BasePayloadAttributes::default();
    attributes.payload_attributes.timestamp = timestamp;
    attributes.payload_attributes.prev_randao = l1_header.mix_hash;
    attributes.payload_attributes.suggested_fee_recipient = Predeploys::SEQUENCER_FEE_VAULT;
    attributes.transactions = Some(vec![l1_info, base_time]);
    attributes.no_tx_pool = Some(true);
    attributes.gas_limit = Some(system_config.gas_limit);
    attributes
}

fn initial_state() -> (B256, MemoryTrieDBProvider) {
    let storage = vec![(
        Nibbles::unpack(keccak256(BaseTime::ADMIN_SLOT.to_be_bytes::<32>())),
        rlp(&U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice())),
    )];
    let (storage_root, mut trie_nodes) = trie(storage);

    let proxy = BaseTime::proxy_bytecode();
    let accounts = vec![
        (
            Nibbles::unpack(keccak256(Predeploys::BASE_TIME)),
            rlp(&TrieAccount {
                nonce: 1,
                storage_root,
                code_hash: keccak256(&proxy),
                ..Default::default()
            }),
        ),
        (
            Nibbles::unpack(keccak256(Predeploys::L2_TO_L1_MESSAGE_PASSER)),
            rlp(&TrieAccount {
                nonce: 1,
                storage_root: EMPTY_ROOT_HASH,
                code_hash: keccak256([]),
                ..Default::default()
            }),
        ),
    ];
    let (state_root, state_nodes) = trie(accounts);
    trie_nodes.extend(state_nodes);
    let bytecodes = BTreeMap::from([
        (keccak256(&proxy), proxy),
        (BaseTime::IMPLEMENTATION_CODE_HASH, BaseTime::implementation_bytecode()),
    ]);
    (state_root, MemoryTrieDBProvider { trie_nodes: Arc::new(RwLock::new(trie_nodes)), bytecodes })
}

fn trie(mut leaves: Vec<(Nibbles, Bytes)>) -> (B256, BTreeMap<B256, Bytes>) {
    leaves.sort_by_key(|(path, _)| *path);
    let paths = leaves.iter().map(|(path, _)| *path).collect();
    let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(paths));
    for (path, value) in leaves {
        builder.add_leaf(path, &value);
    }
    let root = builder.root();
    let nodes = builder
        .take_proof_nodes()
        .into_inner()
        .into_values()
        .map(|value| (keccak256(value.as_ref()), value))
        .collect();
    (root, nodes)
}

fn ordered_trie(values: &[Bytes]) -> (B256, BTreeMap<B256, Bytes>) {
    let mut builder = ordered_trie_with_encoder(values, |value, buffer| {
        buffer.put_slice(value.as_ref());
    });
    let root = builder.root();
    let nodes = builder
        .take_proof_nodes()
        .into_inner()
        .into_values()
        .map(|value| (keccak256(value.as_ref()), value))
        .collect();
    (root, nodes)
}

fn rlp(value: &impl Encodable) -> Bytes {
    let mut encoded = Vec::with_capacity(value.length());
    value.encode(&mut encoded);
    encoded.into()
}

fn encode_base_transaction(transaction: &BaseTxEnvelope) -> Bytes {
    let mut encoded = Vec::new();
    transaction.encode_2718(&mut encoded);
    encoded.into()
}

fn encode_l1_transaction(transaction: &TxEnvelope) -> Bytes {
    let mut encoded = Vec::new();
    transaction.encode_2718(&mut encoded);
    encoded.into()
}

fn encode_receipt(receipt: &BaseReceiptEnvelope) -> Bytes {
    let mut encoded = Vec::new();
    receipt.encode_2718(&mut encoded);
    encoded.into()
}

fn insert_header(store: &mut PreimageStore, header: &Header) {
    insert_keccak(store, rlp(header).to_vec());
}

fn insert_provider(store: &mut PreimageStore, provider: &MemoryTrieDBProvider) {
    for bytes in provider.trie_nodes.read().unwrap().values() {
        insert_keccak(store, bytes.to_vec());
    }
    for bytes in provider.bytecodes.values() {
        insert_keccak(store, bytes.to_vec());
    }
}

fn insert_nodes(store: &mut PreimageStore, nodes: BTreeMap<B256, Bytes>) {
    for bytes in nodes.into_values() {
        insert_keccak(store, bytes.to_vec());
    }
}

fn insert_keccak(store: &mut PreimageStore, value: Vec<u8>) {
    store.save_preimage(PreimageKey::new_keccak256(*keccak256(&value)), value).unwrap();
}

fn insert_local(store: &mut PreimageStore, key: U256, value: Vec<u8>) {
    store.preimage_map.insert(PreimageKey::new_local(key.saturating_to()), value);
}
