//! Enclave-native proof and configuration types.
//!
//! Proof types mirror their counterparts in `base-proof-primitives` but are
//! owned by the enclave crate so that changes to the host-side types (e.g.
//! adding fields to `ProofRequest`) do not alter the enclave binary and
//! therefore do not change the PCR0 measurement.
//!
//! Configuration types provide binary serialization that produces deterministic
//! config hashes for chain identification.
//!
//! We use custom types for `PerChainConfig` and related genesis types because:
//! - We need `B256` for Scalar (not `U256`)
//! - We need exact control over binary serialization order for hash compatibility
//! - `Overhead` exists in the struct but is forced to zero and excluded from binary

use alloy_eips::eip1898::BlockNumHash;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_consensus_genesis::{BaseFeeConfig, BaseHardforkConfig, HardForkConfig, RollupConfig};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

const VERSION_0: u64 = 0;
const MARSHAL_BINARY_SIZE: usize = 212;

/// A block identifier containing both hash and number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BlockId {
    /// The block hash.
    pub hash: B256,
    /// The block number.
    pub number: u64,
}

impl Default for BlockId {
    fn default() -> Self {
        Self { hash: B256::ZERO, number: 0 }
    }
}

/// System configuration at genesis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GenesisSystemConfig {
    /// The address of the batch submitter.
    pub batcher_addr: Address,
    /// The L1 fee overhead (forced to zero).
    pub overhead: B256,
    /// The L1 fee scalar.
    pub scalar: B256,
    /// The gas limit for L2 blocks.
    pub gas_limit: u64,
}

impl Default for GenesisSystemConfig {
    fn default() -> Self {
        Self {
            batcher_addr: Address::ZERO,
            overhead: B256::ZERO,
            scalar: B256::ZERO,
            gas_limit: 30_000_000,
        }
    }
}

/// Genesis block configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Genesis {
    /// The L1 block at genesis.
    pub l1: BlockId,
    /// The L2 block at genesis.
    pub l2: BlockId,
    /// The timestamp of the L2 genesis block.
    #[serde(default)]
    pub l2_time: u64,
    /// The system configuration at genesis.
    pub system_config: GenesisSystemConfig,
}

/// Per-chain configuration that uniquely identifies a chain.
///
/// This is the core configuration type that gets hashed to produce a unique
/// chain identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PerChainConfig {
    /// The chain ID.
    pub chain_id: U256,
    /// The genesis configuration.
    pub genesis: Genesis,
    /// The target block time in seconds.
    ///
    /// Note: This field is NOT included in the binary serialization or config hash.
    /// It is forced to 1 by `force_defaults()` for canonical hashing.
    pub block_time: u64,
    /// The address of the deposit contract on L1.
    pub deposit_contract_address: Address,
    /// The address of the `SystemConfig` contract on L1.
    pub l1_system_config_address: Address,
}

impl Default for PerChainConfig {
    fn default() -> Self {
        Self {
            chain_id: U256::from(1),
            genesis: Genesis::default(),
            block_time: 2,
            deposit_contract_address: Address::ZERO,
            l1_system_config_address: Address::ZERO,
        }
    }
}

impl PerChainConfig {
    /// Create a `PerChainConfig` from a [`RollupConfig`].
    ///
    /// Returns `None` if the rollup config is missing `genesis.system_config`.
    #[must_use]
    pub fn from_rollup_config(cfg: &RollupConfig) -> Option<Self> {
        let sc = cfg.genesis.system_config.as_ref()?;
        Some(Self {
            chain_id: U256::from(cfg.l2_chain_id.id()),
            genesis: Genesis {
                l1: BlockId { hash: cfg.genesis.l1.hash, number: cfg.genesis.l1.number },
                l2: BlockId { hash: cfg.genesis.l2.hash, number: cfg.genesis.l2.number },
                l2_time: cfg.genesis.l2_time,
                system_config: GenesisSystemConfig {
                    batcher_addr: sc.batcher_address,
                    overhead: B256::ZERO,
                    scalar: B256::from(sc.scalar.to_be_bytes::<32>()),
                    gas_limit: sc.gas_limit,
                },
            },
            block_time: cfg.block_time,
            deposit_contract_address: cfg.deposit_contract_address,
            l1_system_config_address: cfg.l1_system_config_address,
        })
    }

    /// Serialize the config to its canonical binary format.
    ///
    /// Binary layout (all big-endian, 212 bytes total):
    /// ```text
    /// Offset | Size | Field
    /// -------|------|------
    /// 0      | 8    | version (always 0)
    /// 8      | 32   | chain_id (left-padded)
    /// 40     | 32   | genesis.l1.hash
    /// 72     | 32   | genesis.l2.hash
    /// 104    | 8    | genesis.l2_time
    /// 112    | 20   | genesis.system_config.batcher_addr
    /// 132    | 32   | genesis.system_config.scalar
    /// 164    | 8    | genesis.system_config.gas_limit
    /// 172    | 20   | deposit_contract_address
    /// 192    | 20   | l1_system_config_address
    ///        | 212  | TOTAL
    /// ```
    #[must_use]
    pub fn marshal_binary(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(MARSHAL_BINARY_SIZE);

        data.extend_from_slice(&VERSION_0.to_be_bytes());
        data.extend_from_slice(&self.chain_id.to_be_bytes::<32>());
        data.extend_from_slice(self.genesis.l1.hash.as_slice());
        data.extend_from_slice(self.genesis.l2.hash.as_slice());
        data.extend_from_slice(&self.genesis.l2_time.to_be_bytes());
        data.extend_from_slice(self.genesis.system_config.batcher_addr.as_slice());
        data.extend_from_slice(self.genesis.system_config.scalar.as_slice());
        data.extend_from_slice(&self.genesis.system_config.gas_limit.to_be_bytes());
        data.extend_from_slice(self.deposit_contract_address.as_slice());
        data.extend_from_slice(self.l1_system_config_address.as_slice());

        debug_assert_eq!(data.len(), MARSHAL_BINARY_SIZE);
        data
    }

    /// Compute the keccak256 hash of the binary-serialized config.
    ///
    /// This produces a deterministic hash suitable for on-chain verification.
    ///
    /// Note: For canonical hashes, call [`force_defaults()`](Self::force_defaults)
    /// first to ensure deterministic values.
    #[must_use]
    pub fn hash(&self) -> B256 {
        keccak256(self.marshal_binary())
    }

    /// Apply forced defaults for canonical hashing.
    ///
    /// These values are forced to ensure deterministic hashing regardless
    /// of what values were originally provided:
    /// - `block_time`: Always 1
    /// - `genesis.l2.number`: Always 0
    /// - `genesis.system_config.overhead`: Always zero
    pub const fn force_defaults(&mut self) {
        self.block_time = 1;
        self.genesis.l2.number = 0;
        self.genesis.system_config.overhead = B256::ZERO;
    }

    /// Converts this per-chain configuration into a full [`RollupConfig`] with default fork
    /// settings (all forks active at genesis).
    #[must_use]
    pub fn to_rollup_config(&self) -> RollupConfig {
        RollupConfig {
            l1_chain_id: 1,
            l2_chain_id: alloy_chains::Chain::from_id(self.chain_id.to::<u64>()),
            genesis: self.to_chain_genesis(),
            block_time: self.block_time,
            max_sequencer_drift: 600,
            seq_window_size: 3600,
            channel_timeout: 300,
            granite_channel_timeout: 300,
            deposit_contract_address: self.deposit_contract_address,
            l1_system_config_address: self.l1_system_config_address,
            batch_inbox_address: Address::ZERO,
            protocol_versions_address: Address::ZERO,
            blobs_enabled_l1_timestamp: Some(0),
            hardforks: HardForkConfig {
                regolith_time: Some(0),
                canyon_time: Some(0),
                delta_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: Some(0),
                granite_time: Some(0),
                holocene_time: Some(0),
                pectra_blob_schedule_time: None,
                isthmus_time: Some(0),
                jovian_time: Some(0),
                base: BaseHardforkConfig { v1: Some(0) },
            },
            chain_op_config: BaseFeeConfig::base_mainnet(),
        }
    }

    /// Convert our Genesis to `base_consensus_genesis::ChainGenesis`.
    const fn to_chain_genesis(&self) -> base_consensus_genesis::ChainGenesis {
        base_consensus_genesis::ChainGenesis {
            l1: BlockNumHash { hash: self.genesis.l1.hash, number: self.genesis.l1.number },
            l2: BlockNumHash { hash: self.genesis.l2.hash, number: self.genesis.l2.number },
            l2_time: self.genesis.l2_time,
            system_config: Some(base_consensus_genesis::SystemConfig {
                batcher_address: self.genesis.system_config.batcher_addr,
                overhead: U256::ZERO,
                scalar: U256::from_be_bytes(self.genesis.system_config.scalar.0),
                gas_limit: self.genesis.system_config.gas_limit,
                base_fee_scalar: None,
                blob_base_fee_scalar: None,
                eip1559_denominator: None,
                eip1559_elasticity: None,
                operator_fee_scalar: None,
                operator_fee_constant: None,
                da_footprint_gas_scalar: None,
                min_base_fee: None,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Proof types
// ---------------------------------------------------------------------------

/// ECDSA signature length in bytes (r: 32 + s: 32 + v: 1).
pub const ECDSA_SIGNATURE_LENGTH: usize = 65;

/// Base length of the proof journal without intermediate roots:
/// address(20) + 5 × bytes32(32) + 2 × uint64(8) = 196 bytes.
pub const PROOF_JOURNAL_BASE_LENGTH: usize = 196;

/// The `AggregateVerifier` contract journal encoding.
///
/// Serializes proposal fields into the byte format expected by on-chain verification:
///
/// ```text
/// prover(20) || l1OriginHash(32) || prevOutputRoot(32)
///   || startingL2Block(8) || outputRoot(32) || endingL2Block(8)
///   || intermediateRoots(32*N) || configHash(32) || imageHash(32)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofJournal {
    /// The proposer address.
    pub proposer: Address,
    /// The L1 origin block hash.
    pub l1_origin_hash: B256,
    /// The previous output root hash.
    pub prev_output_root: B256,
    /// The starting L2 block number.
    pub starting_l2_block: u64,
    /// The output root hash.
    pub output_root: B256,
    /// The ending L2 block number.
    pub ending_l2_block: u64,
    /// Intermediate output roots for aggregate proposals.
    pub intermediate_roots: Vec<B256>,
    /// The config hash.
    pub config_hash: B256,
    /// The TEE image hash.
    pub tee_image_hash: B256,
}

impl ProofJournal {
    /// Encode the journal into the ABI-packed byte format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut data =
            Vec::with_capacity(PROOF_JOURNAL_BASE_LENGTH + 32 * self.intermediate_roots.len());

        data.extend_from_slice(self.proposer.as_slice());
        data.extend_from_slice(self.l1_origin_hash.as_slice());
        data.extend_from_slice(self.prev_output_root.as_slice());
        data.extend_from_slice(&self.starting_l2_block.to_be_bytes());
        data.extend_from_slice(self.output_root.as_slice());
        data.extend_from_slice(&self.ending_l2_block.to_be_bytes());
        for root in &self.intermediate_roots {
            data.extend_from_slice(root.as_slice());
        }
        data.extend_from_slice(self.config_hash.as_slice());
        data.extend_from_slice(self.tee_image_hash.as_slice());

        data
    }
}

/// A proposal containing an output root and signature.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Proposal {
    /// The output root hash.
    pub output_root: B256,
    /// The ECDSA signature (65 bytes: r, s, v).
    pub signature: Bytes,
    /// The L1 origin block hash.
    pub l1_origin_hash: B256,
    /// The L1 origin block number.
    pub l1_origin_number: u64,
    /// The L2 block number (ending block of this proposal's range).
    pub l2_block_number: u64,
    /// The previous output root hash.
    pub prev_output_root: B256,
    /// The config hash.
    pub config_hash: B256,
}

/// Result of a TEE proof computation.
///
/// This is the enclave-native equivalent of `ProofResult::Tee` from
/// `base-proof-primitives`, but as a flat struct rather than an enum variant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TeeProofResult {
    /// The aggregated proposal covering the entire proven block range.
    pub aggregate_proposal: Proposal,
    /// The individual per-block proposals that were aggregated.
    pub proposals: Vec<Proposal>,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use base_consensus_registry::Registry;

    use super::*;

    // -----------------------------------------------------------------------
    // ProofJournal tests
    // -----------------------------------------------------------------------

    fn test_journal() -> ProofJournal {
        ProofJournal {
            proposer: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            l1_origin_hash: b256!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
            prev_output_root: b256!(
                "3333333333333333333333333333333333333333333333333333333333333333"
            ),
            starting_l2_block: 999,
            output_root: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            ending_l2_block: 1000,
            intermediate_roots: vec![],
            config_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            tee_image_hash: b256!(
                "5555555555555555555555555555555555555555555555555555555555555555"
            ),
        }
    }

    #[test]
    fn test_journal_encode_length() {
        let data = test_journal().encode();
        assert_eq!(data.len(), PROOF_JOURNAL_BASE_LENGTH);
        assert_eq!(data.len(), 196);
    }

    #[test]
    fn test_journal_encode_components() {
        let journal = test_journal();
        let data = journal.encode();

        let mut off = 0;
        assert_eq!(
            &data[off..off + 20],
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266").as_slice()
        );
        off += 20;
        assert_eq!(&data[off..off + 32], journal.l1_origin_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], journal.prev_output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 8], &journal.starting_l2_block.to_be_bytes());
        off += 8;
        assert_eq!(&data[off..off + 32], journal.output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 8], &journal.ending_l2_block.to_be_bytes());
        off += 8;
        assert_eq!(&data[off..off + 32], journal.config_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], journal.tee_image_hash.as_slice());
    }

    #[test]
    fn test_journal_encode_with_intermediate_roots() {
        let journal = ProofJournal {
            proposer: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            l1_origin_hash: B256::ZERO,
            prev_output_root: B256::ZERO,
            starting_l2_block: 0,
            output_root: B256::ZERO,
            ending_l2_block: 0,
            intermediate_roots: vec![
                b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
            config_hash: B256::ZERO,
            tee_image_hash: B256::ZERO,
        };

        let data = journal.encode();
        assert_eq!(data.len(), PROOF_JOURNAL_BASE_LENGTH + 64);

        let ir_offset = 20 + 32 + 32 + 8 + 32 + 8;
        assert_eq!(&data[ir_offset..ir_offset + 32], journal.intermediate_roots[0].as_slice());
        assert_eq!(&data[ir_offset + 32..ir_offset + 64], journal.intermediate_roots[1].as_slice());
    }

    // -----------------------------------------------------------------------
    // PerChainConfig tests
    // -----------------------------------------------------------------------

    fn sample_config() -> PerChainConfig {
        PerChainConfig {
            chain_id: U256::from(8453), // Base
            genesis: Genesis {
                l1: BlockId { hash: B256::repeat_byte(0x11), number: 1 },
                l2: BlockId { hash: B256::repeat_byte(0x22), number: 0 },
                l2_time: 1686789600,
                system_config: GenesisSystemConfig {
                    batcher_addr: address!("5050f69a9786f081509234f1a7f4684b5e5b76c9"),
                    overhead: B256::ZERO,
                    scalar: B256::repeat_byte(0x00),
                    gas_limit: 30_000_000,
                },
            },
            block_time: 1,
            deposit_contract_address: address!("49048044d57e1c92a77f79988d21fa8faf74e97e"),
            l1_system_config_address: address!("73a79fab69143498ed3712e519a88a918e1f4072"),
        }
    }

    #[test]
    fn test_marshal_binary_length() {
        let config = sample_config();
        let binary = config.marshal_binary();
        assert_eq!(binary.len(), MARSHAL_BINARY_SIZE);
        assert_eq!(binary.len(), 212);
    }

    #[test]
    fn test_marshal_binary_version() {
        let config = sample_config();
        let binary = config.marshal_binary();

        let version = u64::from_be_bytes(binary[0..8].try_into().unwrap());
        assert_eq!(version, 0);
    }

    #[test]
    fn test_marshal_binary_chain_id() {
        let config = sample_config();
        let binary = config.marshal_binary();

        let chain_id_bytes: [u8; 32] = binary[8..40].try_into().unwrap();
        let chain_id = U256::from_be_bytes(chain_id_bytes);
        assert_eq!(chain_id, U256::from(8453));
    }

    /// Golden test: verify binary serialization produces the expected output.
    #[test]
    fn test_marshal_binary_golden() {
        let config = sample_config();
        let binary = config.marshal_binary();

        let expected = hex::decode(
            "0000000000000000\
             0000000000000000000000000000000000000000000000000000000000002105\
             1111111111111111111111111111111111111111111111111111111111111111\
             2222222222222222222222222222222222222222222222222222222222222222\
             00000000648a5de0\
             5050f69a9786f081509234f1a7f4684b5e5b76c9\
             0000000000000000000000000000000000000000000000000000000000000000\
             0000000001c9c380\
             49048044d57e1c92a77f79988d21fa8faf74e97e\
             73a79fab69143498ed3712e519a88a918e1f4072",
        )
        .unwrap();

        assert_eq!(binary, expected);
    }

    /// Golden test: verify hash produces the expected output.
    #[test]
    fn test_hash_golden() {
        let config = sample_config();

        let expected = b256!("f914a90550e1b3f9d107005221dc01403f63ee8e12884d71699046ddbd7036b2");

        assert_eq!(config.hash(), expected);
    }

    #[test]
    fn test_hash_deterministic() {
        let config1 = sample_config();
        let config2 = sample_config();

        assert_eq!(config1.hash(), config2.hash());
    }

    #[test]
    fn test_hash_changes_with_chain_id() {
        let config1 = sample_config();
        let mut config2 = sample_config();
        config2.chain_id = U256::from(1);

        assert_ne!(config1.hash(), config2.hash());
    }

    #[test]
    fn test_force_defaults() {
        let mut config = PerChainConfig {
            chain_id: U256::from(8453),
            genesis: Genesis {
                l1: BlockId::default(),
                l2: BlockId {
                    hash: B256::ZERO,
                    number: 100, // Will be forced to 0
                },
                l2_time: 0,
                system_config: GenesisSystemConfig {
                    batcher_addr: Address::ZERO,
                    overhead: B256::repeat_byte(0xff), // Will be forced to zero
                    scalar: B256::ZERO,
                    gas_limit: 30_000_000,
                },
            },
            block_time: 10, // Will be forced to 1
            deposit_contract_address: Address::ZERO,
            l1_system_config_address: Address::ZERO,
        };

        config.force_defaults();

        assert_eq!(config.block_time, 1);
        assert_eq!(config.genesis.l2.number, 0);
        assert_eq!(config.genesis.system_config.overhead, B256::ZERO);
    }

    #[test]
    fn test_json_roundtrip() {
        let config = sample_config();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: PerChainConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_json_snake_case() {
        let config = sample_config();
        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("chain_id"));
        assert!(json.contains("block_time"));
        assert!(json.contains("deposit_contract_address"));
        assert!(json.contains("l1_system_config_address"));
        assert!(json.contains("l2_time"));
        assert!(json.contains("batcher_addr"));
        assert!(json.contains("gas_limit"));
    }

    #[test]
    fn test_to_rollup_config() {
        let config = sample_config();
        let rollup_config = config.to_rollup_config();

        assert_eq!(rollup_config.l2_chain_id.id(), 8453);
        assert_eq!(rollup_config.block_time, config.block_time);
        assert_eq!(rollup_config.deposit_contract_address, config.deposit_contract_address);
        assert_eq!(rollup_config.l1_system_config_address, config.l1_system_config_address);
        assert_eq!(rollup_config.genesis.l1.hash, config.genesis.l1.hash);
        assert_eq!(rollup_config.genesis.l2.hash, config.genesis.l2.hash);
    }

    #[test]
    fn test_to_rollup_config_timing() {
        let config = sample_config();
        let rollup_config = config.to_rollup_config();

        assert_eq!(rollup_config.max_sequencer_drift, 600);
        assert_eq!(rollup_config.seq_window_size, 3600);
        assert_eq!(rollup_config.channel_timeout, 300);
    }

    #[test]
    fn test_to_rollup_config_forks_active_at_genesis() {
        let config = sample_config();
        let rollup_config = config.to_rollup_config();

        assert_eq!(rollup_config.hardforks.canyon_time, Some(0));
        assert_eq!(rollup_config.hardforks.delta_time, Some(0));
        assert_eq!(rollup_config.hardforks.ecotone_time, Some(0));
        assert_eq!(rollup_config.hardforks.fjord_time, Some(0));
        assert_eq!(rollup_config.hardforks.granite_time, Some(0));
        assert_eq!(rollup_config.hardforks.holocene_time, Some(0));
        assert_eq!(rollup_config.hardforks.isthmus_time, Some(0));
        assert_eq!(rollup_config.hardforks.regolith_time, Some(0));
    }

    /// Print config hashes for supported chains so they can be hardcoded in the
    /// enclave server. Run with:
    /// `cargo test -p base-proof-tee-nitro-enclave print_real_config_hashes -- --nocapture --ignored`
    #[test]
    #[ignore]
    fn print_real_config_hashes() {
        let chains: &[(u64, &str)] =
            &[(8453, "Base Mainnet"), (84532, "Base Sepolia"), (11763072, "Sepolia Alpha")];

        for &(chain_id, name) in chains {
            let rollup = Registry::rollup_config(chain_id)
                .unwrap_or_else(|| panic!("missing rollup config for {name} ({chain_id})"));
            let mut per_chain = PerChainConfig::from_rollup_config(rollup)
                .unwrap_or_else(|| panic!("missing system_config for {name} ({chain_id})"));
            per_chain.force_defaults();
            println!("{name} ({chain_id}): {:?}", per_chain.hash());
        }
    }
}
