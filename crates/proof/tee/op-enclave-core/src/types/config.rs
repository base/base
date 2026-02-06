//! Configuration types for chain configuration.
//!
//! These types mirror the Go implementation in `enclave/config.go` and provide
//! binary serialization that produces identical config hashes.
//!
//! We use custom types for `PerChainConfig` and related genesis types because:
//! - Go uses `B256` for Scalar, while `kona_genesis` uses `U256`
//! - We need exact control over binary serialization order for hash compatibility
//! - Go's `Overhead` exists in struct but is forced to zero and not in binary
//!
//! For `RollupConfig`, we re-export from `kona_genesis` to maintain ecosystem compatibility.

use alloy_primitives::{Address, B256, U256, keccak256};
use serde::{Deserialize, Serialize};

// Re-export RollupConfig from kona_genesis for ecosystem compatibility
pub use kona_genesis::RollupConfig;

/// Version constant for binary serialization format.
const VERSION_0: u64 = 0;

/// Total size of marshaled binary output in bytes.
pub const MARSHAL_BINARY_SIZE: usize = 212;

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
        Self {
            hash: B256::ZERO,
            number: 0,
        }
    }
}

/// System configuration at genesis.
///
/// Note: We use custom types here instead of `kona_genesis` types because:
/// - Go uses `B256` for Scalar, while `kona_genesis` uses `U256`
/// - We need exact control over binary serialization order
/// - Go's `Overhead` exists in struct but is forced to zero and not in binary
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
/// chain identifier. The binary serialization format matches the Go implementation
/// exactly to ensure hash compatibility.
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
    /// Serialize the config to binary format matching Go's `MarshalBinary()`.
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

        // Version (8 bytes, big-endian)
        data.extend_from_slice(&VERSION_0.to_be_bytes());

        // Chain ID (32 bytes, big-endian, left-padded)
        // U256::to_be_bytes::<32>() gives identical output to Go's big.Int.Bytes() + left-pad
        data.extend_from_slice(&self.chain_id.to_be_bytes::<32>());

        // Genesis L1 hash (32 bytes)
        data.extend_from_slice(self.genesis.l1.hash.as_slice());

        // Genesis L2 hash (32 bytes)
        data.extend_from_slice(self.genesis.l2.hash.as_slice());

        // Genesis L2 time (8 bytes, big-endian)
        data.extend_from_slice(&self.genesis.l2_time.to_be_bytes());

        // Batcher address (20 bytes)
        data.extend_from_slice(self.genesis.system_config.batcher_addr.as_slice());

        // Scalar (32 bytes)
        data.extend_from_slice(self.genesis.system_config.scalar.as_slice());

        // Gas limit (8 bytes, big-endian)
        data.extend_from_slice(&self.genesis.system_config.gas_limit.to_be_bytes());

        // Deposit contract address (20 bytes)
        data.extend_from_slice(self.deposit_contract_address.as_slice());

        // L1 system config address (20 bytes)
        data.extend_from_slice(self.l1_system_config_address.as_slice());

        debug_assert_eq!(data.len(), MARSHAL_BINARY_SIZE);
        data
    }

    /// Compute the keccak256 hash of the binary-serialized config.
    ///
    /// This produces identical output to Go's `crypto.Keccak256Hash()`.
    ///
    /// Note: For canonical hashes, call [`force_defaults()`](Self::force_defaults)
    /// first to ensure deterministic values.
    #[must_use]
    pub fn hash(&self) -> B256 {
        keccak256(self.marshal_binary())
    }

    /// Apply forced defaults matching Go's `ForceDefaults()`.
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

    /// Convert to `kona_genesis::RollupConfig` using default template.
    ///
    /// This starts with `default_rollup_config()` and overwrites the 5 chain-specific fields:
    /// - `l2_chain_id`
    /// - `genesis`
    /// - `block_time`
    /// - `deposit_contract_address`
    /// - `l1_system_config_address`
    #[must_use]
    pub fn to_rollup_config(&self) -> RollupConfig {
        use crate::config::default_rollup_config;

        let mut cfg = default_rollup_config();

        // Overwrite chain-specific fields
        cfg.l2_chain_id = alloy_chains::Chain::from_id(self.chain_id.to::<u64>());
        cfg.genesis = self.to_chain_genesis();
        cfg.block_time = self.block_time;
        cfg.deposit_contract_address = self.deposit_contract_address;
        cfg.l1_system_config_address = self.l1_system_config_address;

        cfg
    }

    /// Convert our Genesis to `kona_genesis::ChainGenesis`.
    const fn to_chain_genesis(&self) -> kona_genesis::ChainGenesis {
        use alloy_eips::eip1898::BlockNumHash;

        kona_genesis::ChainGenesis {
            l1: BlockNumHash {
                hash: self.genesis.l1.hash,
                number: self.genesis.l1.number,
            },
            l2: BlockNumHash {
                hash: self.genesis.l2.hash,
                number: self.genesis.l2.number,
            },
            l2_time: self.genesis.l2_time,
            system_config: Some(kona_genesis::SystemConfig {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    fn sample_config() -> PerChainConfig {
        PerChainConfig {
            chain_id: U256::from(8453), // Base
            genesis: Genesis {
                l1: BlockId {
                    hash: B256::repeat_byte(0x11),
                    number: 1,
                },
                l2: BlockId {
                    hash: B256::repeat_byte(0x22),
                    number: 0,
                },
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

        // First 8 bytes should be version 0
        let version = u64::from_be_bytes(binary[0..8].try_into().unwrap());
        assert_eq!(version, 0);
    }

    #[test]
    fn test_marshal_binary_chain_id() {
        let config = sample_config();
        let binary = config.marshal_binary();

        // Bytes 8-40 should be chain_id (32 bytes, big-endian, left-padded)
        let chain_id_bytes: [u8; 32] = binary[8..40].try_into().unwrap();
        let chain_id = U256::from_be_bytes(chain_id_bytes);
        assert_eq!(chain_id, U256::from(8453));
    }

    /// Golden test: verify binary serialization matches Go's `MarshalBinary()` output.
    ///
    /// Generated by running: `go test -v -run TestGoldenVectors ./enclave/`
    #[test]
    fn test_marshal_binary_matches_go() {
        let config = sample_config();
        let binary = config.marshal_binary();

        // Expected binary from Go's MarshalBinary()
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

        assert_eq!(
            binary, expected,
            "Binary serialization must match Go implementation"
        );
    }

    /// Golden test: verify hash matches Go's `Hash()` output.
    ///
    /// Generated by running: `go test -v -run TestGoldenVectors ./enclave/`
    #[test]
    fn test_hash_matches_go() {
        let config = sample_config();

        // Expected hash from Go's Hash()
        let expected = b256!("f914a90550e1b3f9d107005221dc01403f63ee8e12884d71699046ddbd7036b2");

        assert_eq!(config.hash(), expected, "Hash must match Go implementation");
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

        // Verify snake_case field names
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
        assert_eq!(
            rollup_config.deposit_contract_address,
            config.deposit_contract_address
        );
        assert_eq!(
            rollup_config.l1_system_config_address,
            config.l1_system_config_address
        );
        assert_eq!(rollup_config.genesis.l1.hash, config.genesis.l1.hash);
        assert_eq!(rollup_config.genesis.l2.hash, config.genesis.l2.hash);
    }

    #[test]
    fn test_rollup_config_default_values() {
        let rollup_config = crate::config::default_rollup_config();

        // Verify timing parameters from Go's L2CoreDeployConfig
        assert_eq!(rollup_config.block_time, 2);
        assert_eq!(rollup_config.max_sequencer_drift, 600);
        assert_eq!(rollup_config.seq_window_size, 3600);
        assert_eq!(rollup_config.channel_timeout, 300);
    }

    #[test]
    fn test_all_forks_active_at_genesis() {
        let rollup_config = crate::config::default_rollup_config();

        // All forks should be active at genesis (time = 0)
        assert_eq!(rollup_config.hardforks.canyon_time, Some(0));
        assert_eq!(rollup_config.hardforks.delta_time, Some(0));
        assert_eq!(rollup_config.hardforks.ecotone_time, Some(0));
        assert_eq!(rollup_config.hardforks.fjord_time, Some(0));
        assert_eq!(rollup_config.hardforks.granite_time, Some(0));
        assert_eq!(rollup_config.hardforks.holocene_time, Some(0));
        assert_eq!(rollup_config.hardforks.isthmus_time, Some(0));

        // Regolith should also be active at genesis
        assert_eq!(rollup_config.hardforks.regolith_time, Some(0));
    }
}
