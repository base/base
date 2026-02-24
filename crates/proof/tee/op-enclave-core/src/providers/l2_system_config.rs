//! L2 system config fetcher implementation.
//!
//! This module provides a system config fetcher for L2 blocks, used by the
//! derivation pipeline to access the system configuration from L2 blocks.

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, U256};
use kona_genesis::{RollupConfig, SystemConfig};
use kona_protocol::{L1BlockInfoIsthmusBaseFields, L1BlockInfoJovianBaseFields, L1BlockInfoTx};

use crate::error::ProviderError;

/// A fetcher for L2 system configuration from L2 blocks.
///
/// This struct holds a single L2 block's header and first transaction data,
/// and provides methods to extract the system configuration.
/// It matches Go's `l2SystemConfigFetcher`.
#[derive(Debug, Clone)]
pub struct L2SystemConfigFetcher {
    /// The rollup configuration.
    config: RollupConfig,
    /// The block hash.
    hash: B256,
    /// The block header.
    header: Header,
    /// The first transaction's calldata (deposit tx data).
    first_tx_data: Option<Bytes>,
}

impl L2SystemConfigFetcher {
    /// Creates a new `L2SystemConfigFetcher`.
    ///
    /// # Arguments
    ///
    /// * `config` - The rollup configuration
    /// * `hash` - The block hash
    /// * `header` - The block header
    /// * `first_tx_data` - The first transaction's calldata (should be L1 info deposit)
    #[must_use]
    pub const fn new(
        config: RollupConfig,
        hash: B256,
        header: Header,
        first_tx_data: Option<Bytes>,
    ) -> Self {
        Self {
            config,
            hash,
            header,
            first_tx_data,
        }
    }

    /// Returns the system configuration for the given L2 block hash.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The hash doesn't match
    /// - The block is missing the L1 info deposit transaction
    /// - The L1 block info cannot be parsed
    /// - Genesis hash mismatch
    pub fn system_config_by_l2_hash(&self, hash: B256) -> Result<SystemConfig, ProviderError> {
        if hash != self.hash {
            return Err(ProviderError::BlockNotFound(hash));
        }
        self.block_to_system_config()
    }

    /// Extracts the system configuration from the block.
    fn block_to_system_config(&self) -> Result<SystemConfig, ProviderError> {
        let block_hash = self.hash;
        let block_number = self.header.number;
        let l2_time = self.header.timestamp;

        // Check if this is the genesis block
        if block_number == self.config.genesis.l2.number {
            // Verify genesis hash matches
            if block_hash != self.config.genesis.l2.hash {
                return Err(ProviderError::GenesisHashMismatch {
                    number: block_number,
                    expected: self.config.genesis.l2.hash,
                    actual: block_hash,
                });
            }
            // Return genesis system config
            return self.config.genesis.system_config.ok_or_else(|| {
                ProviderError::L1InfoParseError("genesis system config not set".to_string())
            });
        }

        // Non-genesis block: parse L1 info from deposit tx
        let tx_data = self
            .first_tx_data
            .as_ref()
            .ok_or(ProviderError::MissingL1InfoDeposit(block_hash))?;

        // Parse L1BlockInfo from the deposit transaction data
        let l1_info = L1BlockInfoTx::decode_calldata(tx_data)
            .map_err(|e| ProviderError::L1InfoParseError(e.to_string()))?;

        // Build the system config
        let mut sys_cfg = SystemConfig {
            batcher_address: l1_info.batcher_address(),
            overhead: l1_info.l1_fee_overhead(),
            scalar: U256::from_be_bytes(self.encode_fee_scalar(&l1_info, l2_time)?.0),
            gas_limit: self.header.gas_limit,
            base_fee_scalar: None,
            blob_base_fee_scalar: None,
            eip1559_denominator: None,
            eip1559_elasticity: None,
            operator_fee_scalar: None,
            operator_fee_constant: None,
            da_footprint_gas_scalar: None,
            min_base_fee: None,
        };

        // Add Isthmus operator fee params
        if is_isthmus_but_not_first_block(&self.config, l2_time)
            && let L1BlockInfoTx::Isthmus(info) = &l1_info
        {
            sys_cfg.operator_fee_scalar = Some(info.operator_fee_scalar);
            sys_cfg.operator_fee_constant = Some(info.operator_fee_constant);
        }

        // Add Jovian operator fee params including da_footprint_gas_scalar
        if is_jovian_but_not_first_block(&self.config, l2_time)
            && let L1BlockInfoTx::Jovian(info) = &l1_info
        {
            sys_cfg.operator_fee_scalar = Some(info.operator_fee_scalar());
            sys_cfg.operator_fee_constant = Some(info.operator_fee_constant());
            sys_cfg.da_footprint_gas_scalar = Some(info.da_footprint_gas_scalar());
        }

        Ok(sys_cfg)
    }

    /// Encodes the fee scalar for the system config.
    ///
    /// For Ecotone+ blocks (not activation block), translates the scalar fields
    /// back into the encoded scalar format.
    ///
    /// # Errors
    ///
    /// Returns an error if `blob_base_fee_scalar` or `l1_fee_scalar` exceeds `u32::MAX`.
    fn encode_fee_scalar(
        &self,
        l1_info: &L1BlockInfoTx,
        l2_time: u64,
    ) -> Result<B256, ProviderError> {
        if is_ecotone_but_not_first_block(&self.config, l2_time) {
            // Encode v1 scalar format:
            // byte 0: version (1)
            // bytes 24-28: blob_base_fee_scalar (big-endian u32)
            // bytes 28-32: base_fee_scalar (big-endian u32)
            let mut encoded = [0u8; 32];
            encoded[0] = 1; // version 1

            // Per OP Stack spec, base_fee_scalar and blob_base_fee_scalar are always u32 values.
            // The U256 return type is for API compatibility.
            let blob_scalar: u32 = l1_info.blob_base_fee_scalar().try_into().map_err(|_| {
                ProviderError::FeeScalarOverflow {
                    field: "blob_base_fee_scalar",
                    value: l1_info.blob_base_fee_scalar(),
                }
            })?;
            let base_scalar: u32 = l1_info.l1_fee_scalar().try_into().map_err(|_| {
                ProviderError::FeeScalarOverflow {
                    field: "base_fee_scalar",
                    value: l1_info.l1_fee_scalar(),
                }
            })?;

            encoded[24..28].copy_from_slice(&blob_scalar.to_be_bytes());
            encoded[28..32].copy_from_slice(&base_scalar.to_be_bytes());
            Ok(B256::from(encoded))
        } else {
            // Pre-Ecotone or Ecotone activation block: use raw scalar from L1FeeScalar
            Ok(B256::from(l1_info.l1_fee_scalar().to_be_bytes::<32>()))
        }
    }

    /// Returns the block hash.
    #[must_use]
    pub const fn hash(&self) -> B256 {
        self.hash
    }

    /// Returns a reference to the header.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// Returns a reference to the rollup config.
    #[must_use]
    pub const fn config(&self) -> &RollupConfig {
        &self.config
    }
}

/// Checks if Ecotone is active but this is not the activation block.
const fn is_ecotone_but_not_first_block(config: &RollupConfig, l2_time: u64) -> bool {
    is_fork_active_but_not_activation(config.hardforks.ecotone_time, l2_time)
}

/// Checks if Isthmus is active but this is not the activation block.
const fn is_isthmus_but_not_first_block(config: &RollupConfig, l2_time: u64) -> bool {
    is_fork_active_but_not_activation(config.hardforks.isthmus_time, l2_time)
}

/// Checks if Jovian is active but this is not the activation block.
const fn is_jovian_but_not_first_block(config: &RollupConfig, l2_time: u64) -> bool {
    is_fork_active_but_not_activation(config.hardforks.jovian_time, l2_time)
}

/// Helper to check if a fork is active but not the activation block.
const fn is_fork_active_but_not_activation(fork_time: Option<u64>, l2_time: u64) -> bool {
    matches!(fork_time, Some(t) if l2_time > t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_rollup_config;
    use crate::providers::test_utils::test_header;
    use alloy_primitives::{address, b256, hex};

    // Test vectors from kona-protocol crate - these are real L1BlockInfo calldata bytes
    // from mainnet/testnet blocks.

    /// Bedrock format calldata (260 bytes): selector + 8x 32-byte ABI-encoded slots
    /// Selector: 0x015d8eb9 (keccak256("setL1BlockValues(uint64,uint64,uint256,bytes32,uint64,bytes32,uint256,uint256)"))
    const BEDROCK_CALLDATA: [u8; 260] = hex!(
        "015d8eb9000000000000000000000000000000000000000000000000000000000117c4eb0000000000000000000000000000000000000000000000000000000065280377000000000000000000000000000000000000000000000000000000026d05d953392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc00000000000000000000000000000000000000000000000000000000000000040000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f3298500000000000000000000000000000000000000000000000000000000000000bc00000000000000000000000000000000000000000000000000000000000a6fe0"
    );

    /// Ecotone format calldata (164 bytes): selector + packed binary fields
    /// Selector: 0x440a5e20 (`keccak256("setL1BlockValuesEcotone()")`)
    const ECOTONE_CALLDATA: [u8; 164] = hex!(
        "440a5e2000000558000c5fc5000000000000000500000000661c277300000000012bec20000000000000000000000000000000000000000000000000000000026e9f109900000000000000000000000000000000000000000000000000000000000000011c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add30000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
    );

    #[test]
    fn test_l1_block_info_decode_bedrock_format() {
        // Decode Bedrock calldata and verify all fields
        let l1_info = L1BlockInfoTx::decode_calldata(&Bytes::from_static(&BEDROCK_CALLDATA))
            .expect("valid bedrock calldata");

        // Expected values from the raw hex above
        assert_eq!(
            l1_info.batcher_address(),
            address!("6887246668a3b87f54deb3b94ba47a6f63f32985")
        );
        assert_eq!(l1_info.sequence_number(), 4);
        assert_eq!(l1_info.l1_fee_overhead(), U256::from(0xbc));
        assert_eq!(l1_info.l1_fee_scalar(), U256::from(0xa6fe0));

        // Bedrock doesn't have blob base fee
        assert_eq!(l1_info.blob_base_fee(), U256::ZERO);
        assert_eq!(l1_info.blob_base_fee_scalar(), U256::ZERO);

        // Verify block hash
        let expected_hash =
            b256!("392012032675be9f94aae5ab442de73c5f4fb1bf30fa7dd0d2442239899a40fc");
        assert_eq!(l1_info.block_hash(), expected_hash);
    }

    #[test]
    fn test_l1_block_info_decode_ecotone_format() {
        // Decode Ecotone calldata and verify all fields
        let l1_info = L1BlockInfoTx::decode_calldata(&Bytes::from_static(&ECOTONE_CALLDATA))
            .expect("valid ecotone calldata");

        // Expected values from the raw hex above
        assert_eq!(
            l1_info.batcher_address(),
            address!("6887246668a3b87f54deb3b94ba47a6f63f32985")
        );
        assert_eq!(l1_info.sequence_number(), 5);

        // Ecotone-specific scalar fields (packed as u32)
        // From bytes 4-8: 0x00000558 = 1368 (base_fee_scalar)
        // From bytes 8-12: 0x000c5fc5 = 810949 (blob_base_fee_scalar)
        assert_eq!(l1_info.l1_fee_scalar(), U256::from(1368u32));
        assert_eq!(l1_info.blob_base_fee_scalar(), U256::from(810949u32));

        // Blob base fee should be 1 (from the 32-byte slot)
        assert_eq!(l1_info.blob_base_fee(), U256::from(1));

        // Verify block hash
        let expected_hash =
            b256!("1c4c84c50740386c7dc081efddd644405f04cde73e30a2e381737acce9f5add3");
        assert_eq!(l1_info.block_hash(), expected_hash);
    }

    #[test]
    fn test_system_config_from_ecotone_l1_block_info() {
        // Integration test: parse Ecotone calldata through L2SystemConfigFetcher
        // and verify the scalar encoding matches Go behavior
        let mut config = default_rollup_config();

        // Set Ecotone as active (but not activation block)
        config.hardforks.ecotone_time = Some(100);
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = B256::ZERO;

        let block_hash = B256::repeat_byte(0xAA);
        // Block timestamp must be > ecotone_time to not be the activation block
        let header = test_header(100, 200);

        let fetcher = L2SystemConfigFetcher::new(
            config,
            block_hash,
            header,
            Some(Bytes::from_static(&ECOTONE_CALLDATA)),
        );

        let sys_cfg = fetcher
            .system_config_by_l2_hash(block_hash)
            .expect("should extract system config");

        // Verify batcher address extracted correctly
        assert_eq!(
            sys_cfg.batcher_address,
            address!("6887246668a3b87f54deb3b94ba47a6f63f32985")
        );

        // Verify the scalar is encoded in v1 format:
        // byte 0: version (1)
        // bytes 24-28: blob_base_fee_scalar (810949 = 0x000c5fc5)
        // bytes 28-32: base_fee_scalar (1368 = 0x00000558)
        let scalar_bytes = sys_cfg.scalar.to_be_bytes::<32>();
        assert_eq!(scalar_bytes[0], 1, "scalar version should be 1 for ecotone");

        // Extract blob_base_fee_scalar from bytes 24-28
        let blob_scalar = u32::from_be_bytes([
            scalar_bytes[24],
            scalar_bytes[25],
            scalar_bytes[26],
            scalar_bytes[27],
        ]);
        assert_eq!(blob_scalar, 810949, "blob_base_fee_scalar should match");

        // Extract base_fee_scalar from bytes 28-32
        let base_scalar = u32::from_be_bytes([
            scalar_bytes[28],
            scalar_bytes[29],
            scalar_bytes[30],
            scalar_bytes[31],
        ]);
        assert_eq!(base_scalar, 1368, "base_fee_scalar should match");
    }

    #[test]
    fn test_system_config_from_bedrock_l1_block_info() {
        // Integration test: parse Bedrock calldata through L2SystemConfigFetcher
        let mut config = default_rollup_config();

        // Pre-Ecotone (Bedrock era)
        config.hardforks.ecotone_time = None;
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = B256::ZERO;

        let block_hash = B256::repeat_byte(0xBB);
        let header = test_header(100, 200);

        let fetcher = L2SystemConfigFetcher::new(
            config,
            block_hash,
            header,
            Some(Bytes::from_static(&BEDROCK_CALLDATA)),
        );

        let sys_cfg = fetcher
            .system_config_by_l2_hash(block_hash)
            .expect("should extract system config");

        // Verify batcher address extracted correctly
        assert_eq!(
            sys_cfg.batcher_address,
            address!("6887246668a3b87f54deb3b94ba47a6f63f32985")
        );

        // For Bedrock, scalar is the raw l1_fee_scalar value (0xa6fe0)
        assert_eq!(sys_cfg.scalar, U256::from(0xa6fe0));

        // Overhead should be extracted
        assert_eq!(sys_cfg.overhead, U256::from(0xbc));
    }

    #[test]
    fn test_genesis_block_returns_genesis_config() {
        let mut config = default_rollup_config();
        let genesis_hash = B256::repeat_byte(0xAA);
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = genesis_hash;

        let header = test_header(0, 0);
        let fetcher = L2SystemConfigFetcher::new(config.clone(), genesis_hash, header, None);

        let result = fetcher.system_config_by_l2_hash(genesis_hash);
        assert!(result.is_ok());

        let sys_cfg = result.unwrap();
        assert_eq!(
            sys_cfg.gas_limit,
            config.genesis.system_config.unwrap().gas_limit
        );
    }

    #[test]
    fn test_genesis_hash_mismatch() {
        let mut config = default_rollup_config();
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = B256::repeat_byte(0xAA);

        let wrong_hash = B256::repeat_byte(0xBB);
        let header = test_header(0, 0);
        let fetcher = L2SystemConfigFetcher::new(config, wrong_hash, header, None);

        let result = fetcher.system_config_by_l2_hash(wrong_hash);
        assert!(matches!(
            result,
            Err(ProviderError::GenesisHashMismatch { .. })
        ));
    }

    #[test]
    fn test_block_not_found() {
        let config = default_rollup_config();
        let hash = B256::repeat_byte(0xAA);
        let wrong_hash = B256::repeat_byte(0xBB);
        let header = test_header(100, 1000);
        let fetcher = L2SystemConfigFetcher::new(config, hash, header, None);

        let result = fetcher.system_config_by_l2_hash(wrong_hash);
        assert!(matches!(result, Err(ProviderError::BlockNotFound(h)) if h == wrong_hash));
    }

    #[test]
    fn test_missing_l1_info_deposit() {
        let mut config = default_rollup_config();
        config.genesis.l2.number = 0;
        config.genesis.l2.hash = B256::repeat_byte(0x00);

        let hash = B256::repeat_byte(0xAA);
        let header = test_header(100, 1000); // Not genesis block
        let fetcher = L2SystemConfigFetcher::new(config, hash, header, None);

        let result = fetcher.system_config_by_l2_hash(hash);
        assert!(matches!(
            result,
            Err(ProviderError::MissingL1InfoDeposit(_))
        ));
    }

    #[test]
    fn test_fork_detection_not_active() {
        assert!(!is_fork_active_but_not_activation(None, 100));
    }

    #[test]
    fn test_fork_detection_at_activation() {
        // At exactly activation time, should return false (is activation block)
        assert!(!is_fork_active_but_not_activation(Some(100), 100));
    }

    #[test]
    fn test_fork_detection_after_activation() {
        // After activation time, should return true
        assert!(is_fork_active_but_not_activation(Some(100), 102));
    }

    #[test]
    fn test_fork_detection_before_activation() {
        assert!(!is_fork_active_but_not_activation(Some(100), 50));
    }

    #[test]
    fn test_is_jovian_but_not_first_block() {
        let mut config = default_rollup_config();

        // Jovian not configured
        config.hardforks.jovian_time = None;
        assert!(!is_jovian_but_not_first_block(&config, 100));

        // Before activation
        config.hardforks.jovian_time = Some(100);
        assert!(!is_jovian_but_not_first_block(&config, 50));

        // At activation (first block)
        assert!(!is_jovian_but_not_first_block(&config, 100));

        // After activation (not first block)
        assert!(is_jovian_but_not_first_block(&config, 101));
    }
}
