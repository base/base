//! Rollup Config Types

use alloy_chains::Chain;
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::Address;
use base_alloy_chains::{BaseUpgrade, BaseUpgrades};

use crate::{BaseFeeConfig, ChainGenesis, HardForkConfig, base_fee_config};

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

/// The Rollup configuration.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct RollupConfig {
    /// The genesis state of the rollup.
    pub genesis: ChainGenesis,
    /// The block time of the L2, in seconds.
    pub block_time: u64,
    /// Sequencer batches may not be more than `MaxSequencerDrift` seconds after
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
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "chain_id_as_u64", deserialize_with = "chain_id_from_u64")
    )]
    pub l2_chain_id: Chain,
    /// Hardfork timestamps.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub hardforks: HardForkConfig,
    /// `batch_inbox_address` is the L1 address that batches are sent to.
    pub batch_inbox_address: Address,
    /// `deposit_contract_address` is the L1 address that deposits are sent to.
    pub deposit_contract_address: Address,
    /// `l1_system_config_address` is the L1 address that the system config is stored at.
    pub l1_system_config_address: Address,
    /// `protocol_versions_address` is the L1 address that the protocol versions are stored at.
    pub protocol_versions_address: Address,
    /// `blobs_enabled_l1_timestamp` is the timestamp to start reading blobs as a batch data
    /// source. Optional.
    #[cfg_attr(
        feature = "serde",
        serde(rename = "blobs_data", skip_serializing_if = "Option::is_none")
    )]
    pub blobs_enabled_l1_timestamp: Option<u64>,
    /// `chain_op_config` is the chain-specific EIP1559 config for the rollup.
    #[cfg_attr(feature = "serde", serde(default = "BaseFeeConfig::base_mainnet"))]
    pub chain_op_config: BaseFeeConfig,
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for RollupConfig {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        use base_alloy_chains::BaseChainConfig;
        let chain_op_config = match u32::arbitrary(u)? % 2 {
            0 => BaseFeeConfig::from(BaseChainConfig::mainnet()),
            _ => BaseFeeConfig::from(BaseChainConfig::sepolia()),
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
            hardforks: HardForkConfig::arbitrary(u)?,
            batch_inbox_address: Address::arbitrary(u)?,
            deposit_contract_address: Address::arbitrary(u)?,
            l1_system_config_address: Address::arbitrary(u)?,
            protocol_versions_address: Address::arbitrary(u)?,
            blobs_enabled_l1_timestamp: Option::<u64>::arbitrary(u)?,
            chain_op_config,
        })
    }
}

// Need to manually implement Default because [`BaseFeeParams`] has no Default impl.
impl Default for RollupConfig {
    fn default() -> Self {
        Self {
            genesis: ChainGenesis::default(),
            block_time: 0,
            max_sequencer_drift: 0,
            seq_window_size: 0,
            channel_timeout: 0,
            granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: 0,
            l2_chain_id: Chain::from_id(0),
            hardforks: HardForkConfig::default(),
            batch_inbox_address: Address::ZERO,
            deposit_contract_address: Address::ZERO,
            l1_system_config_address: Address::ZERO,
            protocol_versions_address: Address::ZERO,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: base_fee_config(0),
        }
    }
}

impl RollupConfig {
    /// Returns true if Regolith is active at the given timestamp.
    pub fn is_regolith_active(&self, timestamp: u64) -> bool {
        self.hardforks.regolith_time.is_some_and(|t| timestamp >= t)
            || self.is_canyon_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Regolith block.
    pub fn is_first_regolith_block(&self, timestamp: u64) -> bool {
        self.is_regolith_active(timestamp)
            && !self.is_regolith_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Canyon is active at the given timestamp.
    pub fn is_canyon_active(&self, timestamp: u64) -> bool {
        self.hardforks.canyon_time.is_some_and(|t| timestamp >= t)
            || self.is_delta_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Canyon block.
    pub fn is_first_canyon_block(&self, timestamp: u64) -> bool {
        self.is_canyon_active(timestamp)
            && !self.is_canyon_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Delta is active at the given timestamp.
    pub fn is_delta_active(&self, timestamp: u64) -> bool {
        self.hardforks.delta_time.is_some_and(|t| timestamp >= t)
            || self.is_ecotone_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Delta block.
    pub fn is_first_delta_block(&self, timestamp: u64) -> bool {
        self.is_delta_active(timestamp)
            && !self.is_delta_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Ecotone is active at the given timestamp.
    pub fn is_ecotone_active(&self, timestamp: u64) -> bool {
        self.hardforks.ecotone_time.is_some_and(|t| timestamp >= t)
            || self.is_fjord_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Ecotone block.
    pub fn is_first_ecotone_block(&self, timestamp: u64) -> bool {
        self.is_ecotone_active(timestamp)
            && !self.is_ecotone_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Fjord is active at the given timestamp.
    pub fn is_fjord_active(&self, timestamp: u64) -> bool {
        self.hardforks.fjord_time.is_some_and(|t| timestamp >= t)
            || self.is_granite_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Fjord block.
    pub fn is_first_fjord_block(&self, timestamp: u64) -> bool {
        self.is_fjord_active(timestamp)
            && !self.is_fjord_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Granite is active at the given timestamp.
    pub fn is_granite_active(&self, timestamp: u64) -> bool {
        self.hardforks.granite_time.is_some_and(|t| timestamp >= t)
            || self.is_holocene_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Granite block.
    pub fn is_first_granite_block(&self, timestamp: u64) -> bool {
        self.is_granite_active(timestamp)
            && !self.is_granite_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Holocene is active at the given timestamp.
    pub fn is_holocene_active(&self, timestamp: u64) -> bool {
        self.hardforks.holocene_time.is_some_and(|t| timestamp >= t)
            || self.is_isthmus_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Holocene block.
    pub fn is_first_holocene_block(&self, timestamp: u64) -> bool {
        self.is_holocene_active(timestamp)
            && !self.is_holocene_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if the pectra blob schedule is active at the given timestamp.
    pub fn is_pectra_blob_schedule_active(&self, timestamp: u64) -> bool {
        self.hardforks.pectra_blob_schedule_time.is_some_and(|t| timestamp >= t)
    }

    /// Returns true if the timestamp marks the first pectra blob schedule block.
    pub fn is_first_pectra_blob_schedule_block(&self, timestamp: u64) -> bool {
        self.is_pectra_blob_schedule_active(timestamp)
            && !self.is_pectra_blob_schedule_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Isthmus is active at the given timestamp.
    pub fn is_isthmus_active(&self, timestamp: u64) -> bool {
        self.hardforks.isthmus_time.is_some_and(|t| timestamp >= t)
            || self.is_jovian_active(timestamp)
    }

    /// Returns true if the timestamp marks the first Isthmus block.
    pub fn is_first_isthmus_block(&self, timestamp: u64) -> bool {
        self.is_isthmus_active(timestamp)
            && !self.is_isthmus_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Jovian is active at the given timestamp.
    pub fn is_jovian_active(&self, timestamp: u64) -> bool {
        self.hardforks.jovian_time.is_some_and(|t| timestamp >= t)
    }

    /// Returns true if the timestamp marks the first Jovian block.
    pub fn is_first_jovian_block(&self, timestamp: u64) -> bool {
        self.is_jovian_active(timestamp)
            && !self.is_jovian_active(timestamp.saturating_sub(self.block_time))
    }

    /// Returns true if Base V1 is active at the given timestamp.
    pub fn is_base_v1_active(&self, timestamp: u64) -> bool {
        self.hardforks.base.v1.is_some_and(|t| timestamp >= t)
    }

    /// Returns true if the timestamp marks the first Base V1 block.
    pub fn is_first_base_v1_block(&self, timestamp: u64) -> bool {
        self.is_base_v1_active(timestamp)
            && !self.is_base_v1_active(timestamp.saturating_sub(self.block_time))
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
    /// Computes a block number from a timestamp, relative to the L2 genesis time and the block
    /// time.
    ///
    /// This function assumes that the timestamp is aligned with the block time, and uses floor
    /// division in its computation.
    pub const fn block_number_from_timestamp(&self, timestamp: u64) -> u64 {
        timestamp.saturating_sub(self.genesis.l2_time).saturating_div(self.block_time)
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
}

impl EthereumHardforks for RollupConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Berlin {
            // We assume that Base chains were launched with all forks before Berlin activated.
            ForkCondition::Block(0)
        } else if fork <= EthereumHardfork::Paris {
            // Bedrock activates all hardforks up to Paris.
            self.upgrade_activation(BaseUpgrade::Bedrock)
        } else if fork <= EthereumHardfork::Shanghai {
            // Canyon activates Shanghai hardfork.
            self.upgrade_activation(BaseUpgrade::Canyon)
        } else if fork <= EthereumHardfork::Cancun {
            // Ecotone activates Cancun hardfork.
            self.upgrade_activation(BaseUpgrade::Ecotone)
        } else if fork <= EthereumHardfork::Prague {
            // Isthmus activates Prague hardfork.
            self.upgrade_activation(BaseUpgrade::Isthmus)
        } else {
            ForkCondition::Never
        }
    }
}

impl BaseUpgrades for RollupConfig {
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
        match fork {
            BaseUpgrade::Bedrock => ForkCondition::Block(0),
            BaseUpgrade::Regolith => self
                .hardforks
                .regolith_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Canyon)),
            BaseUpgrade::Canyon => self
                .hardforks
                .canyon_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Ecotone)),
            BaseUpgrade::Ecotone => self
                .hardforks
                .ecotone_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Fjord)),
            BaseUpgrade::Fjord => self
                .hardforks
                .fjord_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Granite)),
            BaseUpgrade::Granite => self
                .hardforks
                .granite_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Holocene)),
            BaseUpgrade::Holocene => self
                .hardforks
                .holocene_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Isthmus)),
            BaseUpgrade::Isthmus => self
                .hardforks
                .isthmus_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Jovian)),
            BaseUpgrade::Jovian => self
                .hardforks
                .jovian_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            // V1 is standalone: not part of the Base upgrade cascade chain. It only activates
            // when explicitly configured and never implies (or is implied by) Jovian being active.
            BaseUpgrade::V1 => {
                self.hardforks.base.v1.map(ForkCondition::Timestamp).unwrap_or(ForkCondition::Never)
            }
            _ => ForkCondition::Never,
        }
    }
}

impl RollupConfig {
    const BASE_V1_ACTIVATION_BANNER: &str = include_str!("../static/base_v1_activation_banner.txt");

    /// Logs hardfork activation when building or processing the first block of a fork.
    pub fn log_upgrade_activation(&self, block_number: u64, timestamp: u64) {
        if self.is_first_ecotone_block(timestamp) {
            tracing::info!(target: "upgrades", block_number, "Activating ecotone upgrade");
        } else if self.is_first_fjord_block(timestamp) {
            tracing::info!(target: "upgrades", block_number, "Activating fjord upgrade");
        } else if self.is_first_granite_block(timestamp) {
            tracing::info!(target: "upgrades", block_number, "Activating granite upgrade");
        } else if self.is_first_holocene_block(timestamp) {
            tracing::info!(target: "upgrades", block_number, "Activating holocene upgrade");
        } else if self.is_first_isthmus_block(timestamp) {
            tracing::info!(target: "upgrades", block_number, "Activating isthmus upgrade");
        } else if self.is_first_jovian_block(timestamp) {
            tracing::info!(target: "upgrades", block_number, "Activating jovian upgrade");
        } else if self.is_first_base_v1_block(timestamp) {
            for line in Self::BASE_V1_ACTIVATION_BANNER.lines() {
                tracing::info!(target: "upgrades", "{line}");
            }
            tracing::info!(target: "upgrades", block_number, "Activating base v1 upgrade");
        }
    }
}

/// Serializes a [`Chain`] as its numeric chain ID.
///
/// `alloy_chains::Chain` serializes named chains (e.g. Base Sepolia) as a string like
/// `"base-sepolia"`, but external consumers such as op-batcher expect a plain integer.
/// This helper forces numeric serialization for all chains.
#[cfg(feature = "serde")]
fn chain_id_as_u64<S: serde::Serializer>(chain: &Chain, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(chain.id())
}

/// Deserializes a [`Chain`] from its numeric chain ID.
#[cfg(feature = "serde")]
fn chain_id_from_u64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Chain, D::Error> {
    let id = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    Ok(Chain::from_id(id))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "serde")]
    use alloy_eips::BlockNumHash;
    use alloy_primitives::address;
    #[cfg(feature = "serde")]
    use alloy_primitives::{U256, b256};

    use super::*;

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_rollup_config() {
        use arbitrary::Arbitrary;
        use rand::Rng;
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        RollupConfig::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    fn test_regolith_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_regolith_active(0));
        config.hardforks.regolith_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(!config.is_regolith_active(9));
    }

    #[test]
    fn test_canyon_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_canyon_active(0));
        config.hardforks.canyon_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(!config.is_canyon_active(9));
    }

    #[test]
    fn test_delta_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_delta_active(0));
        config.hardforks.delta_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(!config.is_delta_active(9));
    }

    #[test]
    fn test_ecotone_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_ecotone_active(0));
        config.hardforks.ecotone_time = Some(10);
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
        config.hardforks.fjord_time = Some(10);
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
        config.hardforks.granite_time = Some(10);
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
        config.hardforks.holocene_time = Some(10);
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
    fn test_pectra_blob_schedule_active() {
        let mut config = RollupConfig::default();
        config.hardforks.pectra_blob_schedule_time = Some(10);
        // Pectra blob schedule is a unique fork, not included in the hierarchical ordering. Its
        // activation does not imply the activation of any other forks.
        assert!(!config.is_regolith_active(10));
        assert!(!config.is_canyon_active(10));
        assert!(!config.is_delta_active(10));
        assert!(!config.is_ecotone_active(10));
        assert!(!config.is_fjord_active(10));
        assert!(!config.is_granite_active(10));
        assert!(!config.is_holocene_active(0));
        assert!(config.is_pectra_blob_schedule_active(10));
        assert!(!config.is_pectra_blob_schedule_active(9));
    }

    #[test]
    fn test_isthmus_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_isthmus_active(0));
        config.hardforks.isthmus_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(config.is_holocene_active(10));
        assert!(!config.is_pectra_blob_schedule_active(10));
        assert!(config.is_isthmus_active(10));
        assert!(!config.is_isthmus_active(9));
    }

    #[test]
    fn test_jovian_active() {
        let mut config = RollupConfig::default();
        assert!(!config.is_jovian_active(0));
        config.hardforks.jovian_time = Some(10);
        assert!(config.is_regolith_active(10));
        assert!(config.is_canyon_active(10));
        assert!(config.is_delta_active(10));
        assert!(config.is_ecotone_active(10));
        assert!(config.is_fjord_active(10));
        assert!(config.is_granite_active(10));
        assert!(config.is_holocene_active(10));
        assert!(!config.is_pectra_blob_schedule_active(10));
        assert!(config.is_isthmus_active(10));
        assert!(config.is_jovian_active(10));
        assert!(!config.is_jovian_active(9));
        assert!(!config.is_base_v1_active(10));
    }

    #[test]
    fn test_base_v1_active() {
        use crate::BaseHardforkConfig;
        let mut config = RollupConfig::default();
        assert!(!config.is_base_v1_active(0));
        config.hardforks.base = BaseHardforkConfig { v1: Some(10) };
        // V1 does not cascade upward to existing forks
        assert!(!config.is_regolith_active(10));
        assert!(!config.is_canyon_active(10));
        assert!(!config.is_jovian_active(10));
        assert!(config.is_base_v1_active(10));
        assert!(!config.is_base_v1_active(9));
    }

    #[test]
    fn test_is_first_fork_block() {
        use crate::BaseHardforkConfig;
        let cfg = RollupConfig {
            hardforks: HardForkConfig {
                regolith_time: Some(10),
                canyon_time: Some(20),
                delta_time: Some(30),
                ecotone_time: Some(40),
                fjord_time: Some(50),
                granite_time: Some(60),
                holocene_time: Some(70),
                pectra_blob_schedule_time: Some(80),
                isthmus_time: Some(90),
                jovian_time: Some(100),
                base: BaseHardforkConfig { v1: Some(110) },
            },
            block_time: 2,
            ..Default::default()
        };

        // Regolith
        assert!(!cfg.is_first_regolith_block(8));
        assert!(cfg.is_first_regolith_block(10));
        assert!(!cfg.is_first_regolith_block(12));

        // Canyon
        assert!(!cfg.is_first_canyon_block(18));
        assert!(cfg.is_first_canyon_block(20));
        assert!(!cfg.is_first_canyon_block(22));

        // Delta
        assert!(!cfg.is_first_delta_block(28));
        assert!(cfg.is_first_delta_block(30));
        assert!(!cfg.is_first_delta_block(32));

        // Ecotone
        assert!(!cfg.is_first_ecotone_block(38));
        assert!(cfg.is_first_ecotone_block(40));
        assert!(!cfg.is_first_ecotone_block(42));

        // Fjord
        assert!(!cfg.is_first_fjord_block(48));
        assert!(cfg.is_first_fjord_block(50));
        assert!(!cfg.is_first_fjord_block(52));

        // Granite
        assert!(!cfg.is_first_granite_block(58));
        assert!(cfg.is_first_granite_block(60));
        assert!(!cfg.is_first_granite_block(62));

        // Holocene
        assert!(!cfg.is_first_holocene_block(68));
        assert!(cfg.is_first_holocene_block(70));
        assert!(!cfg.is_first_holocene_block(72));

        // Pectra blob schedule
        assert!(!cfg.is_first_pectra_blob_schedule_block(78));
        assert!(cfg.is_first_pectra_blob_schedule_block(80));
        assert!(!cfg.is_first_pectra_blob_schedule_block(82));

        // Isthmus
        assert!(!cfg.is_first_isthmus_block(88));
        assert!(cfg.is_first_isthmus_block(90));
        assert!(!cfg.is_first_isthmus_block(92));

        // Jovian
        assert!(!cfg.is_first_jovian_block(98));
        assert!(cfg.is_first_jovian_block(100));
        assert!(!cfg.is_first_jovian_block(102));

        // Base V1
        assert!(!cfg.is_first_base_v1_block(108));
        assert!(cfg.is_first_base_v1_block(110));
        assert!(!cfg.is_first_base_v1_block(112));
    }

    #[test]
    fn test_granite_channel_timeout() {
        let mut config = RollupConfig {
            channel_timeout: 100,
            hardforks: HardForkConfig { granite_time: Some(10), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(config.channel_timeout(0), 100);
        assert_eq!(config.channel_timeout(10), GRANITE_CHANNEL_TIMEOUT);
        config.hardforks.granite_time = None;
        assert_eq!(config.channel_timeout(10), 100);
    }

    #[test]
    fn test_max_sequencer_drift() {
        let mut config = RollupConfig { max_sequencer_drift: 100, ..Default::default() };
        assert_eq!(config.max_sequencer_drift(0), 100);
        config.hardforks.fjord_time = Some(10);
        assert_eq!(config.max_sequencer_drift(0), 100);
        assert_eq!(config.max_sequencer_drift(10), FJORD_MAX_SEQUENCER_DRIFT);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_reference_rollup_config() {
        use crate::SystemConfig;

        let raw: &str = r#"
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
              "gasLimit": 30000000,
              "baseFeeScalar": 1234,
              "blobBaseFeeScalar": 5678,
              "eip1559Denominator": 10,
              "eip1559Elasticity": 20,
              "operatorFeeScalar": 30,
              "operatorFeeConstant": 40,
              "minBaseFee": 50,
              "daFootprintGasScalar": 10
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
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
            }
        }
        "#;

        let expected = RollupConfig {
            genesis: ChainGenesis {
                l1: BlockNumHash {
                    hash: b256!("481724ee99b1f4cb71d826e2ec5a37265f460e9b112315665c977f4050b0af54"),
                    number: 10,
                },
                l2: BlockNumHash {
                    hash: b256!("88aedfbf7dea6bfa2c4ff315784ad1a7f145d8f650969359c003bbed68c87631"),
                    number: 0,
                },
                l2_time: 1725557164,
                system_config: Some(SystemConfig {
                    batcher_address: address!("c81f87a644b41e49b3221f41251f15c6cb00ce03"),
                    overhead: U256::ZERO,
                    scalar: U256::from(0xf4240),
                    gas_limit: 30_000_000,
                    base_fee_scalar: Some(1234),
                    blob_base_fee_scalar: Some(5678),
                    eip1559_denominator: Some(10),
                    eip1559_elasticity: Some(20),
                    operator_fee_scalar: Some(30),
                    operator_fee_constant: Some(40),
                    min_base_fee: Some(50),
                    da_footprint_gas_scalar: Some(10),
                }),
            },
            block_time: 2,
            max_sequencer_drift: 600,
            seq_window_size: 3600,
            channel_timeout: 300,
            granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: 3151908,
            l2_chain_id: Chain::from_id(1337),
            hardforks: HardForkConfig {
                regolith_time: Some(0),
                canyon_time: Some(0),
                delta_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: Some(0),
                ..Default::default()
            },
            batch_inbox_address: address!("ff00000000000000000000000000000000042069"),
            deposit_contract_address: address!("08073dc48dde578137b8af042bcbc1c2491f1eb2"),
            l1_system_config_address: address!("94ee52a9d8edd72a85dea7fae3ba6d75e4bf1710"),
            protocol_versions_address: Address::ZERO,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: base_fee_config(0),
        };

        let deserialized: RollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn test_rollup_config_unknown_field() {
        let raw: &str = r#"
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
          "protocol_versions_address": "0x0000000000000000000000000000000000000000",
          "chain_op_config": {
            "eip1559_elasticity": 100,
            "eip1559_denominator": 100,
            "eip1559_denominator_canyon": 100
          },
          "unknown_field": "unknown"
        }
        "#;

        let err = serde_json::from_str::<RollupConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_l2_chain_id_serializes_as_number() {
        // Named chains (e.g. Base Sepolia, ID 84532) must serialize as a numeric JSON value,
        // not as the string "base-sepolia". op-batcher and other Go consumers expect *big.Int.
        let cfg = RollupConfig { l2_chain_id: Chain::from_id(84532), ..Default::default() };
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json["l2_chain_id"].is_number(),
            "l2_chain_id must serialize as a number, got: {}",
            json["l2_chain_id"]
        );
        assert_eq!(json["l2_chain_id"], 84532u64);

        // Round-trip: deserializing from a numeric l2_chain_id must also work.
        let round_tripped: RollupConfig = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped.l2_chain_id.id(), 84532);
    }

    #[test]
    fn test_compute_block_number_from_time() {
        let cfg = RollupConfig {
            genesis: ChainGenesis { l2_time: 10, ..Default::default() },
            block_time: 2,
            ..Default::default()
        };

        assert_eq!(cfg.block_number_from_timestamp(20), 5);
        assert_eq!(cfg.block_number_from_timestamp(30), 10);
    }
}
