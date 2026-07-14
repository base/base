//! Rollup Config Types

use alloy_chains::Chain;
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::Address;

use crate::{
    BaseUpgrade, ChainGenesis, FeeConfig, RuntimeUpgradeRegistry, UpgradeActivation,
    UpgradeActivationSink, UpgradeConfig,
};

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
    #[cfg_attr(
        feature = "serde",
        serde(default = "RollupConfig::default_granite_channel_timeout")
    )]
    pub granite_channel_timeout: u64,
    /// The L1 chain ID
    pub l1_chain_id: u64,
    /// The L2 chain ID
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "chain_id_as_u64", deserialize_with = "chain_id_from_u64")
    )]
    pub l2_chain_id: Chain,
    /// Upgrade timestamps.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub upgrades: UpgradeConfig,
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
    #[cfg_attr(feature = "serde", serde(default = "FeeConfig::base_mainnet"))]
    pub chain_op_config: FeeConfig,
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for RollupConfig {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            genesis: ChainGenesis::arbitrary(u)?,
            block_time: u.arbitrary()?,
            max_sequencer_drift: u.arbitrary()?,
            seq_window_size: u.arbitrary()?,
            channel_timeout: u.arbitrary()?,
            granite_channel_timeout: u.arbitrary()?,
            l1_chain_id: u.arbitrary()?,
            l2_chain_id: u.arbitrary()?,
            upgrades: UpgradeConfig::arbitrary(u)?,
            batch_inbox_address: Address::arbitrary(u)?,
            deposit_contract_address: Address::arbitrary(u)?,
            l1_system_config_address: Address::arbitrary(u)?,
            protocol_versions_address: Address::arbitrary(u)?,
            blobs_enabled_l1_timestamp: Option::<u64>::arbitrary(u)?,
            chain_op_config: FeeConfig::base_mainnet(),
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
            granite_channel_timeout: Self::GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: 0,
            l2_chain_id: Chain::from_id(0),
            upgrades: UpgradeConfig::default(),
            batch_inbox_address: Address::ZERO,
            deposit_contract_address: Address::ZERO,
            l1_system_config_address: Address::ZERO,
            protocol_versions_address: Address::ZERO,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: FeeConfig::base_mainnet(),
        }
    }
}

impl EthereumHardforks for RollupConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        // Helper: cascade through the Base upgrade chain, returning the first set timestamp.
        let cascade = |starting: &[Option<u64>]| -> ForkCondition {
            if let Some(ts) = starting.iter().flatten().next() {
                return ForkCondition::Timestamp(*ts);
            }
            ForkCondition::Never
        };

        if fork <= EthereumHardfork::Berlin {
            // Pre-Bedrock Ethereum forks all activate at block 0 on Base chains.
            ForkCondition::Block(0)
        } else if fork <= EthereumHardfork::Paris {
            // Bedrock activates everything from London through Paris at block 0.
            ForkCondition::Block(0)
        } else if fork <= EthereumHardfork::Shanghai {
            // Canyon activates Shanghai; cascade through later Base upgrades if unset.
            cascade(&[
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Canyon),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Ecotone),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Fjord),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Granite),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Holocene),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Isthmus),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Jovian),
            ])
        } else if fork <= EthereumHardfork::Cancun {
            // Ecotone activates Cancun; cascade through later Base upgrades if unset.
            cascade(&[
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Ecotone),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Fjord),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Granite),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Holocene),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Isthmus),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Jovian),
            ])
        } else if fork <= EthereumHardfork::Prague {
            // Isthmus activates Prague; cascade through later Base upgrades if unset.
            cascade(&[
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Isthmus),
                self.contract_upgrade_activation_timestamp(BaseUpgrade::Jovian),
            ])
        } else if fork <= EthereumHardfork::Osaka {
            self.contract_upgrade_activation_timestamp(BaseUpgrade::Azul)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never)
        } else {
            ForkCondition::Never
        }
    }
}

macro_rules! rollup_fork_methods {
    ($(
        $active:ident,
        $first:ident,
        [$($timestamp:tt)+],
        $name:literal
        $(, implies $next:ident)?;
    )*) => {
        $(
            #[doc = concat!("Returns true if ", $name, " is active at the given timestamp.")]
            pub fn $active(&self, timestamp: u64) -> bool {
                self.$($timestamp)+.is_some_and(|t| timestamp >= t) $(|| self.$next(timestamp))?
            }

            #[doc = concat!(
                "Returns true if the block at `timestamp` is the first ",
                $name,
                " block when compared against the parent timestamp.",
            )]
            pub fn $first(&self, timestamp: u64, parent_timestamp: u64) -> bool {
                self.$active(timestamp) && !self.$active(parent_timestamp)
            }
        )*
    };
}

impl RollupConfig {
    /// Returns this rollup config's runtime-aware activation for a contract upgrade ID.
    pub fn contract_upgrade_activation(&self, upgrade_id: BaseUpgrade) -> UpgradeActivation {
        // Zombie is a permanently-off production gate: runtime overrides cannot activate it.
        if matches!(upgrade_id, BaseUpgrade::Zombie) {
            #[cfg(any(test, feature = "test-utils"))]
            return RuntimeUpgradeRegistry::activation(self.l2_chain_id.id(), upgrade_id)
                .unwrap_or(UpgradeActivation::Never);
            #[cfg(not(any(test, feature = "test-utils")))]
            return UpgradeActivation::Never;
        }
        RuntimeUpgradeRegistry::activation(self.l2_chain_id.id(), upgrade_id)
            .unwrap_or_else(|| self.upgrades.activation(upgrade_id))
    }

    /// Returns this rollup config's runtime-aware activation timestamp for a contract upgrade ID.
    pub fn contract_upgrade_activation_timestamp(&self, upgrade_id: BaseUpgrade) -> Option<u64> {
        self.contract_upgrade_activation(upgrade_id).timestamp()
    }

    /// Applies runtime upgrade overrides to this rollup config's local upgrade view.
    pub fn apply_runtime_upgrade_overrides(&mut self) {
        if let Some(overrides) = RuntimeUpgradeRegistry::overrides(self.l2_chain_id.id()) {
            self.upgrades.apply_activation_overrides(&overrides);
        }
    }

    /// Returns a clone with runtime upgrade overrides materialized into `upgrades`.
    pub fn with_runtime_upgrade_overrides(&self) -> Self {
        let mut config = self.clone();
        config.apply_runtime_upgrade_overrides();
        config
    }

    /// Clears all timestamp-based upgrade activation times.
    pub fn clear_upgrade_activation_timestamps(&mut self) {
        self.upgrades.clear_activation_timestamps();
    }

    /// Clears a timestamp-based upgrade activation time by contract upgrade ID.
    pub const fn clear_upgrade_activation_timestamp(&mut self, upgrade_id: BaseUpgrade) {
        self.upgrades.clear_activation_timestamp(upgrade_id)
    }

    /// Sets a timestamp-based upgrade activation time by contract upgrade ID.
    pub const fn set_upgrade_activation_timestamp(
        &mut self,
        upgrade_id: BaseUpgrade,
        timestamp: u64,
    ) {
        self.upgrades.set_activation_timestamp(upgrade_id, timestamp)
    }

    /// Applies an upgrade activation by contract upgrade ID.
    pub const fn apply_upgrade_activation(
        &mut self,
        upgrade_id: BaseUpgrade,
        activation: UpgradeActivation,
    ) {
        match activation {
            UpgradeActivation::Timestamp(timestamp) => {
                self.set_upgrade_activation_timestamp(upgrade_id, timestamp)
            }
            UpgradeActivation::Never => self.clear_upgrade_activation_timestamp(upgrade_id),
        }
    }

    rollup_fork_methods! {
        is_regolith_active,
        is_first_regolith_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Regolith)],
        "Regolith",
        implies is_canyon_active;

        is_canyon_active,
        is_first_canyon_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Canyon)],
        "Canyon",
        implies is_delta_active;

        is_delta_active,
        is_first_delta_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Delta)],
        "Delta",
        implies is_ecotone_active;

        is_ecotone_active,
        is_first_ecotone_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Ecotone)],
        "Ecotone",
        implies is_fjord_active;

        is_fjord_active,
        is_first_fjord_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Fjord)],
        "Fjord",
        implies is_granite_active;

        is_granite_active,
        is_first_granite_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Granite)],
        "Granite",
        implies is_holocene_active;

        is_holocene_active,
        is_first_holocene_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Holocene)],
        "Holocene",
        implies is_isthmus_active;

        is_pectra_blob_schedule_active,
        is_first_pectra_blob_schedule_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::PectraBlobSchedule)],
        "pectra blob schedule";

        is_isthmus_active,
        is_first_isthmus_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Isthmus)],
        "Isthmus",
        implies is_jovian_active;

        is_jovian_active,
        is_first_jovian_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Jovian)],
        "Jovian";

        is_base_azul_active,
        is_first_base_azul_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Azul)],
        "Base Azul";

        is_beryl_active,
        is_first_beryl_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Beryl)],
        "Beryl";

        is_cobalt_active,
        is_first_cobalt_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Cobalt)],
        "Cobalt";

        is_zombie_active,
        is_first_zombie_block,
        [contract_upgrade_activation_timestamp(BaseUpgrade::Zombie)],
        "Zombie";
    }

    /// Returns the max sequencer drift for the given timestamp.
    pub fn max_sequencer_drift(&self, timestamp: u64) -> u64 {
        if self.is_fjord_active(timestamp) {
            Self::FJORD_MAX_SEQUENCER_DRIFT
        } else {
            self.max_sequencer_drift
        }
    }

    /// Returns the max rlp bytes per channel for the given timestamp.
    pub fn max_rlp_bytes_per_channel(&self, timestamp: u64) -> u64 {
        if self.is_fjord_active(timestamp) {
            Self::MAX_RLP_BYTES_PER_CHANNEL_FJORD
        } else {
            Self::MAX_RLP_BYTES_PER_CHANNEL_BEDROCK
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
    /// Computes the lower-bound block number for a timestamp, relative to the L2 genesis time and
    /// the block time.
    ///
    /// This uses floor division, so multiple blocks can share the same seconds-denominated
    /// timestamp while still mapping to the same lower bound.
    pub const fn block_number_lower_bound_from_timestamp(&self, timestamp: u64) -> u64 {
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

impl RollupConfig {
    /// The max rlp bytes per channel for the Bedrock hardfork.
    pub const MAX_RLP_BYTES_PER_CHANNEL_BEDROCK: u64 = 10_000_000;

    /// The max rlp bytes per channel for the Fjord hardfork.
    pub const MAX_RLP_BYTES_PER_CHANNEL_FJORD: u64 = 100_000_000;

    /// The max sequencer drift when the Fjord hardfork is active.
    pub const FJORD_MAX_SEQUENCER_DRIFT: u64 = 1800;

    /// The channel timeout once the Granite hardfork is active.
    pub const GRANITE_CHANNEL_TIMEOUT: u64 = 50;

    /// Helper method for deserializing a default granite channel timeout.
    #[cfg(feature = "serde")]
    pub const fn default_granite_channel_timeout() -> u64 {
        Self::GRANITE_CHANNEL_TIMEOUT
    }

    /// The activation banner for the Base Azul hardfork, printed when the first block of the fork is built or processed.
    const AZUL_ACTIVATION_BANNER: &str = include_str!("../static/azul_activation_banner.txt");

    /// Logs upgrade activation when the caller knows the actual parent timestamp.
    pub fn log_upgrade_activation(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) {
        let upgrade = if self.is_first_ecotone_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Ecotone)
        } else if self.is_first_fjord_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Fjord)
        } else if self.is_first_granite_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Granite)
        } else if self.is_first_holocene_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Holocene)
        } else if self.is_first_isthmus_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Isthmus)
        } else if self.is_first_jovian_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Jovian)
        } else if self.is_first_base_azul_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Azul)
        } else if self.is_first_beryl_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Beryl)
        } else if self.is_first_cobalt_block(timestamp, parent_timestamp) {
            Some(BaseUpgrade::Cobalt)
        } else {
            None
        };

        let Some(upgrade) = upgrade else {
            return;
        };

        if let BaseUpgrade::Azul = upgrade {
            for line in Self::AZUL_ACTIVATION_BANNER.lines() {
                tracing::info!(target: "upgrades", "{line}");
            }
        }

        tracing::info!(target: "upgrades", block_number, upgrade = upgrade.contract_id(), "Activated upgrade");
    }
}

impl UpgradeActivationSink for RollupConfig {
    type Error = core::convert::Infallible;

    fn apply_activation(
        &mut self,
        upgrade_id: BaseUpgrade,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error> {
        if matches!(upgrade_id, BaseUpgrade::Zombie) {
            return Ok(false);
        }

        self.apply_upgrade_activation(upgrade_id, activation);
        Ok(true)
    }
}

/// Serializes a [`Chain`] as its numeric chain ID.
///
/// `alloy_chains::Chain` serializes named chains (e.g. Base Sepolia) as a string like
/// `"base-sepolia"`, but external Go consumers expect a plain integer.
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
    #[cfg(feature = "serde")]
    use alloy_primitives::{U256, address, b256};
    #[cfg(feature = "arbitrary")]
    use arbitrary::Arbitrary;
    #[cfg(feature = "arbitrary")]
    use rand::Rng;

    use super::*;
    use crate::BaseUpgradeConfig;
    #[cfg(feature = "serde")]
    use crate::SystemConfig;

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_rollup_config() {
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        RollupConfig::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    fn test_is_first_fork_block() {
        let cfg = RollupConfig {
            upgrades: UpgradeConfig {
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
                base: BaseUpgradeConfig { azul: Some(110), beryl: Some(120), cobalt: Some(130) },
            },
            block_time: 2,
            ..Default::default()
        };

        // Regolith
        assert!(!cfg.is_first_regolith_block(8, 6));
        assert!(cfg.is_first_regolith_block(10, 8));
        assert!(!cfg.is_first_regolith_block(12, 10));

        // Canyon
        assert!(!cfg.is_first_canyon_block(18, 16));
        assert!(cfg.is_first_canyon_block(20, 18));
        assert!(!cfg.is_first_canyon_block(22, 20));

        // Delta
        assert!(!cfg.is_first_delta_block(28, 26));
        assert!(cfg.is_first_delta_block(30, 28));
        assert!(!cfg.is_first_delta_block(32, 30));

        // Ecotone
        assert!(!cfg.is_first_ecotone_block(38, 36));
        assert!(cfg.is_first_ecotone_block(40, 38));
        assert!(!cfg.is_first_ecotone_block(42, 40));

        // Fjord
        assert!(!cfg.is_first_fjord_block(48, 46));
        assert!(cfg.is_first_fjord_block(50, 48));
        assert!(!cfg.is_first_fjord_block(52, 50));

        // Granite
        assert!(!cfg.is_first_granite_block(58, 56));
        assert!(cfg.is_first_granite_block(60, 58));
        assert!(!cfg.is_first_granite_block(62, 60));

        // Holocene
        assert!(!cfg.is_first_holocene_block(68, 66));
        assert!(cfg.is_first_holocene_block(70, 68));
        assert!(!cfg.is_first_holocene_block(72, 70));

        // Pectra blob schedule
        assert!(!cfg.is_first_pectra_blob_schedule_block(78, 76));
        assert!(cfg.is_first_pectra_blob_schedule_block(80, 78));
        assert!(!cfg.is_first_pectra_blob_schedule_block(82, 80));

        // Isthmus
        assert!(!cfg.is_first_isthmus_block(88, 86));
        assert!(cfg.is_first_isthmus_block(90, 88));
        assert!(!cfg.is_first_isthmus_block(92, 90));

        // Jovian
        assert!(!cfg.is_first_jovian_block(98, 96));
        assert!(cfg.is_first_jovian_block(100, 98));
        assert!(!cfg.is_first_jovian_block(102, 100));

        // Base Azul
        assert!(!cfg.is_first_base_azul_block(108, 106));
        assert!(cfg.is_first_base_azul_block(110, 108));
        assert!(!cfg.is_first_base_azul_block(112, 110));

        // Beryl
        assert!(!cfg.is_first_beryl_block(118, 116));
        assert!(cfg.is_first_beryl_block(120, 118));
        assert!(!cfg.is_first_beryl_block(122, 120));

        // Cobalt
        assert!(!cfg.is_first_cobalt_block(128, 126));
        assert!(cfg.is_first_cobalt_block(130, 128));
        assert!(!cfg.is_first_cobalt_block(132, 130));
    }

    #[test]
    fn test_first_beryl_block_handles_same_second_boundary() {
        let cfg = RollupConfig {
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { azul: Some(110), beryl: Some(120), cobalt: None },
                ..Default::default()
            },
            block_time: 2,
            ..Default::default()
        };

        assert!(cfg.is_first_beryl_block(120, 118));
        assert!(!cfg.is_first_beryl_block(120, 120));
    }

    #[test]
    fn test_granite_channel_timeout() {
        let mut config = RollupConfig {
            channel_timeout: 100,
            upgrades: UpgradeConfig { granite_time: Some(10), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(config.channel_timeout(0), 100);
        assert_eq!(config.channel_timeout(10), RollupConfig::GRANITE_CHANNEL_TIMEOUT);
        config.upgrades.granite_time = None;
        assert_eq!(config.channel_timeout(10), 100);
    }

    #[test]
    fn test_max_sequencer_drift() {
        let mut config = RollupConfig { max_sequencer_drift: 100, ..Default::default() };
        assert_eq!(config.max_sequencer_drift(0), 100);
        config.upgrades.fjord_time = Some(10);
        assert_eq!(config.max_sequencer_drift(0), 100);
        assert_eq!(config.max_sequencer_drift(10), RollupConfig::FJORD_MAX_SEQUENCER_DRIFT);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_reference_rollup_config() {
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
            granite_channel_timeout: RollupConfig::GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: 3151908,
            l2_chain_id: Chain::from_id(1337),
            upgrades: UpgradeConfig {
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
            chain_op_config: FeeConfig::base_mainnet(),
        };

        let deserialized: RollupConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(deserialized, expected);
    }

    #[test]
    #[cfg(feature = "serde")]
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
        // not as the string "base-sepolia". Go consumers expect *big.Int.
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
    fn test_ethereum_fork_activation() {
        // Pre-Bedrock Ethereum forks always activate at block 0 on Base chains.
        let cfg = RollupConfig::default();
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Berlin), ForkCondition::Block(0));
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Paris), ForkCondition::Block(0));

        // With no timestamps set everything from Shanghai onward is Never.
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Shanghai), ForkCondition::Never);
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Cancun), ForkCondition::Never);
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Prague), ForkCondition::Never);
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Osaka), ForkCondition::Never);

        // Shanghai↔Canyon: canyon_time drives Shanghai activation.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.canyon_time = Some(100);
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Shanghai),
            ForkCondition::Timestamp(100)
        );

        // Delta alone does NOT activate Shanghai (Delta only covers Span Batches, not L1 EIPs).
        let mut cfg = RollupConfig::default();
        cfg.upgrades.delta_time = Some(150);
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Shanghai), ForkCondition::Never);

        // Canyon unset → Shanghai cascades to ecotone_time (skipping delta_time).
        let mut cfg = RollupConfig::default();
        cfg.upgrades.ecotone_time = Some(200);
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Shanghai),
            ForkCondition::Timestamp(200)
        );

        // Cancun↔Ecotone: ecotone_time drives Cancun activation.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.ecotone_time = Some(300);
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Cancun),
            ForkCondition::Timestamp(300)
        );

        // Ecotone unset → Cancun cascades to jovian_time.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.jovian_time = Some(400);
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Cancun),
            ForkCondition::Timestamp(400)
        );

        // Prague↔Isthmus: isthmus_time drives Prague activation.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.isthmus_time = Some(500);
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(500)
        );

        // Isthmus unset → Prague cascades to jovian_time.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.jovian_time = Some(600);
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(600)
        );

        // Osaka↔Azul: azul drives Osaka activation; standalone (not cascaded from Jovian).
        let mut cfg = RollupConfig::default();
        cfg.upgrades.base = BaseUpgradeConfig { azul: Some(700), beryl: None, cobalt: None };
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(700)
        );

        // Beryl follows Azul; Osaka still activates at Azul when both are configured.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.base = BaseUpgradeConfig { azul: Some(700), beryl: Some(800), cobalt: None };
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(700)
        );
        assert!(cfg.is_base_azul_active(800));
        assert!(cfg.is_beryl_active(800));

        // Beryl requires Azul, and does not independently activate Osaka.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.base = BaseUpgradeConfig { azul: None, beryl: Some(800), cobalt: None };
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Osaka), ForkCondition::Never);

        // Jovian set but Azul unset → Osaka is Never.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.jovian_time = Some(900);
        assert_eq!(cfg.ethereum_fork_activation(EthereumHardfork::Osaka), ForkCondition::Never);
    }

    #[test]
    fn set_upgrade_activation_timestamp_updates_osaka_activation() {
        let mut cfg = RollupConfig::default();

        cfg.set_upgrade_activation_timestamp(BaseUpgrade::Azul, 700);

        assert_eq!(cfg.upgrades.base.azul, Some(700));
        assert!(cfg.is_base_azul_active(700));
        assert_eq!(
            cfg.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(700)
        );

        cfg.clear_upgrade_activation_timestamps();

        assert_eq!(cfg.upgrades, UpgradeConfig::default());
    }

    #[test]
    fn runtime_overrides_update_fork_checks_and_materialized_view() {
        let chain_id = 9_100_002;
        crate::RuntimeUpgradeRegistry::clear_chain(chain_id);
        let cfg = RollupConfig {
            l2_chain_id: Chain::from_id(chain_id),
            upgrades: UpgradeConfig { canyon_time: Some(10), ..Default::default() },
            ..Default::default()
        };

        assert!(cfg.is_canyon_active(10));

        crate::RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, BaseUpgrade::Canyon);
        crate::RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);
        crate::RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);
        // The Zombie gate stays off even when a runtime override tries to activate it.
        crate::RuntimeUpgradeRegistry::set_activation_timestamp(
            chain_id,
            BaseUpgrade::Zombie,
            u64::MAX,
        );

        assert!(!cfg.is_canyon_active(10));
        assert!(cfg.is_base_azul_active(42));
        assert!(cfg.is_cobalt_active(84));
        assert!(!cfg.is_zombie_active(84));
        assert!(!cfg.is_zombie_active(u64::MAX));

        let materialized = cfg.with_runtime_upgrade_overrides();
        assert_eq!(materialized.upgrades.canyon_time, None);
        assert_eq!(materialized.upgrades.base.azul, Some(42));
        assert_eq!(materialized.upgrades.base.cobalt, Some(84));
        assert_eq!(
            materialized.contract_upgrade_activation(BaseUpgrade::Zombie),
            UpgradeActivation::Never
        );

        crate::RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn zombie_activation_can_be_enabled_for_testing() {
        let chain_id = 9_100_003;
        crate::RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 21);
        let cfg = RollupConfig { l2_chain_id: Chain::from_id(chain_id), ..Default::default() };
        {
            let _activation =
                crate::RuntimeUpgradeRegistry::activate_zombie_for_testing(chain_id, 42);

            assert!(!cfg.is_zombie_active(41));
            assert!(cfg.is_zombie_active(42));
            assert!(cfg.is_zombie_active(u64::MAX));
            crate::RuntimeUpgradeRegistry::set_activation_timestamp(
                chain_id,
                BaseUpgrade::Azul,
                22,
            );
            {
                let _nested =
                    crate::RuntimeUpgradeRegistry::activate_zombie_for_testing(chain_id, 84);
                assert!(!cfg.is_zombie_active(83));
                assert!(cfg.is_zombie_active(84));
            }
            assert!(cfg.is_zombie_active(42));
        }
        assert_eq!(
            crate::RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(22))
        );
        assert_eq!(crate::RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Zombie), None);
        crate::RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn test_compute_block_number_lower_bound_from_time() {
        let cfg = RollupConfig {
            genesis: ChainGenesis { l2_time: 10, ..Default::default() },
            block_time: 2,
            ..Default::default()
        };

        assert_eq!(cfg.block_number_lower_bound_from_timestamp(20), 5);
        assert_eq!(cfg.block_number_lower_bound_from_timestamp(30), 10);
    }
}
