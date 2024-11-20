//! Rollup Config Types

use crate::{base_fee_params, ChainGenesis};
use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::Address;

/// The max rlp bytes per channel for the Bedrock hardfork.
pub const MAX_RLP_BYTES_PER_CHANNEL_BEDROCK: u64 = 10_000_000;

/// The max rlp bytes per channel for the Fjord hardfork.
pub const MAX_RLP_BYTES_PER_CHANNEL_FJORD: u64 = 100_000_000;

/// The max sequencer drift when the Fjord hardfork is active.
pub const FJORD_MAX_SEQUENCER_DRIFT: u64 = 1800;

/// The channel timeout once the Granite hardfork is active.
pub const GRANITE_CHANNEL_TIMEOUT: u64 = 50;

#[cfg(feature = "serde")]
const fn default_granite_channel_timeout() -> u64 {
    GRANITE_CHANNEL_TIMEOUT
}

/// Returns the rollup config for the given chain ID.
#[deprecated(since = "0.6.7", note = "Use the `op-alloy-registry` crate instead")]
pub const fn rollup_config_from_chain_id(_: u64) -> Result<RollupConfig, &'static str> {
    Err("Use the `op-alloy-registry` crate instead")
}

/// The Rollup configuration.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RollupConfig {
    /// The genesis state of the rollup.
    pub genesis: ChainGenesis,
    /// The block time of the L2, in seconds.
    pub block_time: u64,
    /// Sequencer batches may not be more than MaxSequencerDrift seconds after
    /// the L1 timestamp of the sequencing window end.
    ///
    /// Note: When L1 has many 1 second consecutive blocks, and L2 grows at fixed 2 seconds,
    /// the L2 time may still grow beyond this difference.
    ///
    /// Note: After the Fjord hardfork, this value becomes a constant of `1800`.
    pub max_sequencer_drift: u64,
    /// The sequencer window size.
    pub seq_window_size: u64,
    /// Number of L1 blocks between when a channel can be opened and when it can be closed.
    pub channel_timeout: u64,
    /// The channel timeout after the Granite hardfork.
    #[cfg_attr(feature = "serde", serde(default = "default_granite_channel_timeout"))]
    pub granite_channel_timeout: u64,
    /// The L1 chain ID
    pub l1_chain_id: u64,
    /// The L2 chain ID
    pub l2_chain_id: u64,
    /// Base Fee Params
    #[cfg_attr(feature = "serde", serde(default = "BaseFeeParams::optimism"))]
    pub base_fee_params: BaseFeeParams,
    /// Base fee params post-canyon hardfork
    #[cfg_attr(feature = "serde", serde(default = "BaseFeeParams::optimism_canyon"))]
    pub canyon_base_fee_params: BaseFeeParams,
    /// `regolith_time` sets the activation time of the Regolith network-upgrade:
    /// a pre-mainnet Bedrock change that addresses findings of the Sherlock contest related to
    /// deposit attributes. "Regolith" is the loose deposited rock that sits on top of Bedrock.
    /// Active if regolith_time != None && L2 block timestamp >= Some(regolith_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub regolith_time: Option<u64>,
    /// `canyon_time` sets the activation time of the Canyon network upgrade.
    /// Active if `canyon_time` != None && L2 block timestamp >= Some(canyon_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub canyon_time: Option<u64>,
    /// `delta_time` sets the activation time of the Delta network upgrade.
    /// Active if `delta_time` != None && L2 block timestamp >= Some(delta_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub delta_time: Option<u64>,
    /// `ecotone_time` sets the activation time of the Ecotone network upgrade.
    /// Active if `ecotone_time` != None && L2 block timestamp >= Some(ecotone_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub ecotone_time: Option<u64>,
    /// `fjord_time` sets the activation time of the Fjord network upgrade.
    /// Active if `fjord_time` != None && L2 block timestamp >= Some(fjord_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub fjord_time: Option<u64>,
    /// `granite_time` sets the activation time for the Granite network upgrade.
    /// Active if `granite_time` != None && L2 block timestamp >= Some(granite_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub granite_time: Option<u64>,
    /// `holocene_time` sets the activation time for the Holocene network upgrade.
    /// Active if `holocene_time` != None && L2 block timestamp >= Some(holocene_time), inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub holocene_time: Option<u64>,
    /// `batch_inbox_address` is the L1 address that batches are sent to.
    pub batch_inbox_address: Address,
    /// `deposit_contract_address` is the L1 address that deposits are sent to.
    pub deposit_contract_address: Address,
    /// `l1_system_config_address` is the L1 address that the system config is stored at.
    pub l1_system_config_address: Address,
    /// `protocol_versions_address` is the L1 address that the protocol versions are stored at.
    pub protocol_versions_address: Address,
    /// The superchain config address.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub superchain_config_address: Option<Address>,
    /// `blobs_enabled_l1_timestamp` is the timestamp to start reading blobs as a batch data
    /// source. Optional.
    #[cfg_attr(
        feature = "serde",
        serde(rename = "blobs_data", skip_serializing_if = "Option::is_none")
    )]
    pub blobs_enabled_l1_timestamp: Option<u64>,
    /// `da_challenge_address` is the L1 address that the data availability challenge contract is
    /// stored at.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub da_challenge_address: Option<Address>,
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for RollupConfig {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        use crate::{
            BASE_SEPOLIA_BASE_FEE_PARAMS, OP_MAINNET_BASE_FEE_PARAMS, OP_SEPOLIA_BASE_FEE_PARAMS,
        };
        let params = match u32::arbitrary(u)? % 3 {
            0 => OP_MAINNET_BASE_FEE_PARAMS,
            1 => OP_SEPOLIA_BASE_FEE_PARAMS,
            _ => BASE_SEPOLIA_BASE_FEE_PARAMS,
        };
        Ok(Self {
            genesis: ChainGenesis::arbitrary(u)?,
            block_time: u.arbitrary()?,
            max_sequencer_drift: u.arbitrary()?,
            seq_window_size: u.arbitrary()?,
            channel_timeout: u.arbitrary()?,
            granite_channel_timeout: u.arbitrary()?,
            l1_chain_id: u.arbitrary()?,
            l2_chain_id: u.arbitrary()?,
            base_fee_params: params.as_base_fee_params(),
            canyon_base_fee_params: params.as_canyon_base_fee_params(),
            regolith_time: Option::<u64>::arbitrary(u)?,
            canyon_time: Option::<u64>::arbitrary(u)?,
            delta_time: Option::<u64>::arbitrary(u)?,
            ecotone_time: Option::<u64>::arbitrary(u)?,
            fjord_time: Option::<u64>::arbitrary(u)?,
            granite_time: Option::<u64>::arbitrary(u)?,
            holocene_time: Option::<u64>::arbitrary(u)?,
            batch_inbox_address: Address::arbitrary(u)?,
            deposit_contract_address: Address::arbitrary(u)?,
            l1_system_config_address: Address::arbitrary(u)?,
            protocol_versions_address: Address::arbitrary(u)?,
            superchain_config_address: Option::<Address>::arbitrary(u)?,
            blobs_enabled_l1_timestamp: Option::<u64>::arbitrary(u)?,
            da_challenge_address: Option::<Address>::arbitrary(u)?,
        })
    }
}

// Need to manually implement Default because [`BaseFeeParams`] has no Default impl.
impl Default for RollupConfig {
    fn default() -> Self {
        let config = base_fee_params(10);
        Self {
            genesis: ChainGenesis::default(),
            block_time: 0,
            max_sequencer_drift: 0,
            seq_window_size: 0,
            channel_timeout: 0,
            granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: 0,
            l2_chain_id: 0,
            base_fee_params: config.as_base_fee_params(),
            canyon_base_fee_params: config.as_canyon_base_fee_params(),
            regolith_time: None,
            canyon_time: None,
            delta_time: None,
            ecotone_time: None,
            fjord_time: None,
            granite_time: None,
            holocene_time: None,
            batch_inbox_address: Address::ZERO,
            deposit_contract_address: Address::ZERO,
            l1_system_config_address: Address::ZERO,
            protocol_versions_address: Address::ZERO,
            superchain_config_address: None,
            blobs_enabled_l1_timestamp: None,
            da_challenge_address: None,
        }
    }
}

impl RollupConfig {
    /// Returns true if Regolith is active at the given timestamp.
    pub fn is_regolith_active(&self, timestamp: u64) -> bool {
        self.regolith_time.map_or(false, |t| timestamp >= t) || self.is_canyon_active(timestamp)
    }

    /// Returns true if Canyon is active at the given timestamp.
    pub fn is_canyon_active(&self, timestamp: u64) -> bool {
        self.canyon_time.map_or(false, |t| timestamp >= t) || self.is_delta_active(timestamp)
    }

    /// Returns true if Delta is active at the given timestamp.
    pub fn is_delta_active(&self, timestamp: u64) -> bool {
        self.delta_time.map_or(false, |t| timestamp >= t) || self.is_ecotone_active(timestamp)
    }

    /// Returns true if Ecotone is active at the given timestamp.
    pub fn is_ecotone_active(&self, timestamp: u64) -> bool {
        self.ecotone_time.map_or(false, |t| timestamp >= t) || self.is_fjord_active(timestamp)
    }

    /// Returns true if Fjord is active at the given timestamp.
    pub fn is_fjord_active(&self, timestamp: u64) -> bool {
        self.fjord_time.map_or(false, |t| timestamp >= t) || self.is_granite_active(timestamp)
    }

    /// Returns true if Granite is active at the given timestamp.
    pub fn is_granite_active(&self, timestamp: u64) -> bool {
        self.granite_time.map_or(false, |t| timestamp >= t) || self.is_holocene_active(timestamp)
    }

    /// Returns true if Holocene is active at the given timestamp.
    pub fn is_holocene_active(&self, timestamp: u64) -> bool {
        self.holocene_time.map_or(false, |t| timestamp >= t)
    }

    /// Returns true if a DA Challenge proxy Address is provided in the rollup config and the
    /// address is not zero.
    pub fn is_alt_da_enabled(&self) -> bool {
        self.da_challenge_address.map_or(false, |addr| !addr.is_zero())
    }

    /// Returns the max sequencer drift for the given timestamp.
    pub fn max_sequencer_drift(&self, timestamp: u64) -> u64 {
        if self.is_fjord_active(timestamp) {
            FJORD_MAX_SEQUENCER_DRIFT
        } else {
            self.max_sequencer_drift
        }
    }

    /// Returns the max rlp bytes per channel for the given timestamp.
    pub fn max_rlp_bytes_per_channel(&self, timestamp: u64) -> u64 {
        if self.is_fjord_active(timestamp) {
            MAX_RLP_BYTES_PER_CHANNEL_FJORD
        } else {
            MAX_RLP_BYTES_PER_CHANNEL_BEDROCK
        }
    }

    /// Returns the channel timeout for the given timestamp.
    pub fn channel_timeout(&self, timestamp: u64) -> u64 {
        if self.is_granite_active(timestamp) {
            self.granite_channel_timeout
        } else {
            self.channel_timeout
        }
    }

    /// Checks the scalar value in Ecotone.
    pub fn check_ecotone_l1_system_config_scalar(scalar: [u8; 32]) -> Result<(), &'static str> {
        let version_byte = scalar[0];
        match version_byte {
            0 => {
                if scalar[1..28] != [0; 27] {
                    return Err("Bedrock scalar padding not empty");
                }
                Ok(())
            }
            1 => {
                if scalar[1..24] != [0; 23] {
                    return Err("Invalid version 1 scalar padding");
                }
                Ok(())
            }
            _ => {
                // ignore the event if it's an unknown scalar format
                Err("Unrecognized scalar version")
            }
        }
    }

    /// Returns the [RollupConfig] for the given L2 chain ID.
    #[deprecated(since = "0.6.7", note = "Use the `op-alloy-registry` crate instead")]
    pub const fn from_l2_chain_id(_: u64) -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SystemConfig, OP_MAINNET_BASE_FEE_PARAMS};
    use alloy_eips::BlockNumHash;
    #[cfg(feature = "serde")]
    use alloy_primitives::U256;
    use alloy_primitives::{address, b256};
    use arbitrary::Arbitrary;
    use rand::Rng;

    #[test]
    fn test_arbitrary_rollup_config() {
        let mut bytes = [0u8; 1024];
        rand::thread_rng().fill(bytes.as_mut_slice());
        RollupConfig::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    fn test_regolith_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_regolith_active(0));
        config.regolith_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(!config.is_regolith_active(9));
    }

    #[test]
    fn test_canyon_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_canyon_active(0));
        config.canyon_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(!config.is_canyon_active(9));
    }

    #[test]
    fn test_delta_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_delta_active(0));
        config.delta_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(!config.is_delta_active(9));
    }

    #[test]
    fn test_ecotone_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_ecotone_active(0));
        config.ecotone_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(!config.is_ecotone_active(9));
    }

    #[test]
    fn test_fjord_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_fjord_active(0));
        config.fjord_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(!config.is_fjord_active(9));
    }

    #[test]
    fn test_granite_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_granite_active(0));
        config.granite_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(!config.is_granite_active(9));
    }

    #[test]
    fn test_holocene_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_holocene_active(0));
        config.holocene_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(config.is_holocene_active(10));
        assert!(!config.is_holocene_active(9));
    }

    #[test]
    fn test_alt_da_enabled() {
        let mut config = RollupConfig::default();
        assert!(!config.is_alt_da_enabled());
        config.da_challenge_address = Some(Address::ZERO);
        assert!(!config.is_alt_da_enabled());
        config.da_challenge_address = Some(address!("0000000000000000000000000000000000000001"));
        assert!(config.is_alt_da_enabled());
    }

    #[test]
    fn test_granite_channel_timeout() {
        let mut config =
            RollupConfig { channel_timeout: 100, granite_time: Some(10), ..Default::default() };
        assert_eq!(config.channel_timeout(0), 100);
        assert_eq!(config.channel_timeout(10), GRANITE_CHANNEL_TIMEOUT);
        config.granite_time = None;
        assert_eq!(config.channel_timeout(10), 100);
    }

    #[test]
    fn test_max_sequencer_drift() {
        let mut config = RollupConfig { max_sequencer_drift: 100, ..Default::default() };
        assert_eq!(config.max_sequencer_drift(0), 100);
        config.fjord_time = Some(10);
        assert_eq!(config.max_sequencer_drift(0), 100);
        assert_eq!(config.max_sequencer_drift(10), FJORD_MAX_SEQUENCER_DRIFT);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_reference_rollup_config() {
        // Reference serialized rollup config from the `op-node`.
        let ser_cfg = r#"
{
  "genesis": {
    "l1": {
      "hash": "0x481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54",
      "number": 10
    },
    "l2": {
      "hash": "0x88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631",
      "number": 0
    },
    "l2_time": 1725557164,
    "system_config": {
      "batcherAddr": "0xc81f87a644b41e49b3221f41251f15c6cb00ce03",
      "overhead": "0x0000000000000000000000000000000000000000000000000000000000000000",
      "scalar": "0x00000000000000000000000000000000000000000000000000000000000f4240",
      "gasLimit": 30000000
    }
  },
  "block_time": 2,
  "max_sequencer_drift": 600,
  "seq_window_size": 3600,
  "channel_timeout": 300,
  "l1_chain_id": 3151908,
  "l2_chain_id": 1337,
  "regolith_time": 0,
  "canyon_time": 0,
  "delta_time": 0,
  "ecotone_time": 0,
  "fjord_time": 0,
  "batch_inbox_address": "0xff00000000000000000000000000000000042069",
  "deposit_contract_address": "0x08073dc48dde578137b8af042bcbc1c2491f1eb2",
  "l1_system_config_address": "0x94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710",
  "protocol_versions_address": "0x0000000000000000000000000000000000000000"
}
        "#;
        let config: RollupConfig = serde_json::from_str(ser_cfg).unwrap();

        // Validate standard fields.
        assert_eq!(
            config.genesis,
            ChainGenesis {
                l1: BlockNumHash {
                    hash: b256!("481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54"),
                    number: 10
                },
                l2: BlockNumHash {
                    hash: b256!("88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631"),
                    number: 0
                },
                l2_time: 1725557164,
                system_config: Some(SystemConfig {
                    batcher_address: address!("c81f87a644b41e49b3221f41251f15c6cb00ce03"),
                    overhead: U256::ZERO,
                    scalar: U256::from(0xf4240),
                    gas_limit: 30_000_000,
                    base_fee_scalar: None,
                    blob_base_fee_scalar: None,
                    eip1559_denominator: None,
                    eip1559_elasticity: None,
                })
            }
        );
        assert_eq!(config.block_time, 2);
        assert_eq!(config.max_sequencer_drift, 600);
        assert_eq!(config.seq_window_size, 3600);
        assert_eq!(config.channel_timeout, 300);
        assert_eq!(config.l1_chain_id, 3151908);
        assert_eq!(config.l2_chain_id, 1337);
        assert_eq!(config.regolith_time, Some(0));
        assert_eq!(config.canyon_time, Some(0));
        assert_eq!(config.delta_time, Some(0));
        assert_eq!(config.ecotone_time, Some(0));
        assert_eq!(config.fjord_time, Some(0));
        assert_eq!(
            config.batch_inbox_address,
            address!("ff00000000000000000000000000000000042069")
        );
        assert_eq!(
            config.deposit_contract_address,
            address!("08073dc48dde578137b8af042bcbc1c2491f1eb2")
        );
        assert_eq!(
            config.l1_system_config_address,
            address!("94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710")
        );
        assert_eq!(config.protocol_versions_address, Address::ZERO);

        // Validate non-standard fields.
        assert_eq!(config.granite_channel_timeout, GRANITE_CHANNEL_TIMEOUT);
        assert_eq!(config.base_fee_params, OP_MAINNET_BASE_FEE_PARAMS.as_base_fee_params());
        assert_eq!(
            config.canyon_base_fee_params,
            OP_MAINNET_BASE_FEE_PARAMS.as_canyon_base_fee_params()
        );
    }
}
