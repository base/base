//! Rollup Config Types

use alloy_chains::Chain;
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::Address;
use base_common_chains::{BaseUpgrade, ChainConfig, Upgrades};

use crate::{ChainGenesis, FeeConfig, HardForkConfig};

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
    #[cfg_attr(feature = "serde", serde(default = "FeeConfig::base_mainnet"))]
    pub chain_op_config: FeeConfig,
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for RollupConfig {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        use base_common_chains::ChainConfig;
        let chain_op_config = match u32::arbitrary(u)? % 2 {
            0 => FeeConfig::from(ChainConfig::mainnet()),
            _ => FeeConfig::from(ChainConfig::sepolia()),
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
            granite_channel_timeout: Self::GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: 0,
            l2_chain_id: Chain::from_id(0),
            hardforks: HardForkConfig::default(),
            batch_inbox_address: Address::ZERO,
            deposit_contract_address: Address::ZERO,
            l1_system_config_address: Address::ZERO,
            protocol_versions_address: Address::ZERO,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: FeeConfig::from_chain_id(0),
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

    /// Returns true if Base Azul is active at the given timestamp.
    pub fn is_base_azul_active(&self, timestamp: u64) -> bool {
        self.hardforks.base.azul.is_some_and(|t| timestamp >= t)
    }

    /// Returns true if the timestamp marks the first Base Azul block.
    pub fn is_first_base_azul_block(&self, timestamp: u64) -> bool {
        self.is_base_azul_active(timestamp)
            && !self.is_base_azul_active(timestamp.saturating_sub(self.block_time))
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
        } else if fork <= EthereumHardfork::Osaka {
            // Azul activates Osaka hardfork.
            self.upgrade_activation(BaseUpgrade::Azul)
        } else {
            ForkCondition::Never
        }
    }
}

impl Upgrades for RollupConfig {
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
            // Azul is standalone: not part of the Base upgrade cascade chain. It only activates
            // when explicitly configured and never implies (or is implied by) Jovian being active.
            BaseUpgrade::Azul => self
                .hardforks
                .base
                .azul
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            _ => ForkCondition::Never,
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
        } else if self.is_first_base_azul_block(timestamp) {
            for line in Self::AZUL_ACTIVATION_BANNER.lines() {
                tracing::info!(target: "upgrades", "{line}");
            }
            tracing::info!(target: "upgrades", block_number, "Activating azul upgrade");
        }
    }
}

impl From<&ChainConfig> for RollupConfig {
    fn from(cfg: &ChainConfig) -> Self {
        Self {
            genesis: ChainGenesis::from(cfg),
            block_time: cfg.block_time,
            max_sequencer_drift: cfg.max_sequencer_drift,
            seq_window_size: cfg.seq_window_size,
            channel_timeout: cfg.channel_timeout,
            granite_channel_timeout: Self::GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: cfg.l1_chain_id,
            l2_chain_id: Chain::from_id(cfg.chain_id),
            hardforks: HardForkConfig::from(cfg),
            batch_inbox_address: cfg.batch_inbox_address,
            deposit_contract_address: cfg.deposit_contract_address,
            l1_system_config_address: cfg.system_config_address,
            protocol_versions_address: cfg.protocol_versions_address,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: FeeConfig::from(cfg),
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
    #[cfg(feature = "serde")]
    use alloy_primitives::{U256, address, b256};
    #[cfg(feature = "arbitrary")]
    use arbitrary::Arbitrary;
    #[cfg(feature = "arbitrary")]
    use rand::Rng;

    use super::*;
    use crate::HardforkConfig;
    #[cfg(feature = "serde")]
    use crate::SystemConfig;

    type ActivationCheck = fn(&RollupConfig, u64) -> bool;
    type ActivationSetup = fn(&mut RollupConfig, u64);

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_rollup_config() {
        let mut bytes = [0u8; 1024];
        rand::rng().fill(bytes.as_mut_slice());
        RollupConfig::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
    }

    #[test]
    fn test_hardfork_activation_matrix() {
        const ACTIVATION_TIMESTAMP: u64 = 10;
        const PRE_ACTIVATION_TIMESTAMP: u64 = ACTIVATION_TIMESTAMP - 1;

        let cascade_checks = [
            ("regolith", RollupConfig::is_regolith_active as ActivationCheck),
            ("canyon", RollupConfig::is_canyon_active as ActivationCheck),
            ("delta", RollupConfig::is_delta_active as ActivationCheck),
            ("ecotone", RollupConfig::is_ecotone_active as ActivationCheck),
            ("fjord", RollupConfig::is_fjord_active as ActivationCheck),
            ("granite", RollupConfig::is_granite_active as ActivationCheck),
            ("holocene", RollupConfig::is_holocene_active as ActivationCheck),
            ("isthmus", RollupConfig::is_isthmus_active as ActivationCheck),
            ("jovian", RollupConfig::is_jovian_active as ActivationCheck),
        ];
        let independent_checks = [
            (
                "pectra blob schedule",
                RollupConfig::is_pectra_blob_schedule_active as ActivationCheck,
            ),
            ("base azul", RollupConfig::is_base_azul_active as ActivationCheck),
        ];
        let cascade_cases = [
            (
                "regolith",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.regolith_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
            (
                "canyon",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.canyon_time = Some(timestamp);
                }) as ActivationSetup,
                Some(EthereumHardfork::Shanghai),
            ),
            (
                "delta",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.delta_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
            (
                "ecotone",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.ecotone_time = Some(timestamp);
                }) as ActivationSetup,
                Some(EthereumHardfork::Cancun),
            ),
            (
                "fjord",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.fjord_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
            (
                "granite",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.granite_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
            (
                "holocene",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.holocene_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
            (
                "isthmus",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.isthmus_time = Some(timestamp);
                }) as ActivationSetup,
                Some(EthereumHardfork::Prague),
            ),
            (
                "jovian",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.jovian_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
        ];
        let independent_cases = [
            (
                "pectra blob schedule",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.pectra_blob_schedule_time = Some(timestamp);
                }) as ActivationSetup,
                None,
            ),
            (
                "base azul",
                (|config: &mut RollupConfig, timestamp| {
                    config.hardforks.base = HardforkConfig { azul: Some(timestamp) };
                }) as ActivationSetup,
                Some(EthereumHardfork::Osaka),
            ),
        ];

        for (case_index, (case_name, setup, ethereum_fork)) in cascade_cases.into_iter().enumerate()
        {
            let mut config = RollupConfig::default();

            if let Some(fork) = ethereum_fork {
                assert_eq!(config.ethereum_fork_activation(fork), ForkCondition::Never);
            }

            setup(&mut config, ACTIVATION_TIMESTAMP);

            for (check_index, (check_name, check)) in cascade_checks.iter().enumerate() {
                assert_eq!(
                    check(&config, ACTIVATION_TIMESTAMP),
                    check_index <= case_index,
                    "expected {check_name} activation to match {case_name} cascade",
                );
                assert!(
                    !check(&config, PRE_ACTIVATION_TIMESTAMP),
                    "expected {check_name} to be inactive before {case_name}",
                );
            }

            for (check_name, check) in independent_checks {
                assert!(
                    !check(&config, ACTIVATION_TIMESTAMP),
                    "expected {check_name} to remain inactive when {case_name} activates",
                );
                assert!(
                    !check(&config, PRE_ACTIVATION_TIMESTAMP),
                    "expected {check_name} to be inactive before {case_name}",
                );
            }

            if let Some(fork) = ethereum_fork {
                assert_eq!(
                    config.ethereum_fork_activation(fork),
                    ForkCondition::Timestamp(ACTIVATION_TIMESTAMP)
                );
                assert!(
                    config.ethereum_fork_activation(fork).active_at_timestamp(ACTIVATION_TIMESTAMP)
                );
                assert!(
                    !config
                        .ethereum_fork_activation(fork)
                        .active_at_timestamp(PRE_ACTIVATION_TIMESTAMP)
                );
            }
        }

        for (case_index, (case_name, setup, ethereum_fork)) in
            independent_cases.into_iter().enumerate()
        {
            let mut config = RollupConfig::default();

            if let Some(fork) = ethereum_fork {
                assert_eq!(config.ethereum_fork_activation(fork), ForkCondition::Never);
            }

            setup(&mut config, ACTIVATION_TIMESTAMP);

            for (check_name, check) in cascade_checks {
                assert!(
                    !check(&config, ACTIVATION_TIMESTAMP),
                    "expected {check_name} to remain inactive when {case_name} activates",
                );
                assert!(
                    !check(&config, PRE_ACTIVATION_TIMESTAMP),
                    "expected {check_name} to be inactive before {case_name}",
                );
            }

            for (check_index, (check_name, check)) in independent_checks.into_iter().enumerate() {
                let expected = check_index == case_index;
                assert_eq!(
                    check(&config, ACTIVATION_TIMESTAMP),
                    expected,
                    "expected {check_name} activation to match {case_name}",
                );
                assert!(
                    !check(&config, PRE_ACTIVATION_TIMESTAMP),
                    "expected {check_name} to be inactive before {case_name}",
                );
            }

            if let Some(fork) = ethereum_fork {
                assert_eq!(
                    config.ethereum_fork_activation(fork),
                    ForkCondition::Timestamp(ACTIVATION_TIMESTAMP)
                );
                assert!(
                    config.ethereum_fork_activation(fork).active_at_timestamp(ACTIVATION_TIMESTAMP)
                );
                assert!(
                    !config
                        .ethereum_fork_activation(fork)
                        .active_at_timestamp(PRE_ACTIVATION_TIMESTAMP)
                );
            }
        }
    }

    #[test]
    fn test_is_first_fork_block() {
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
                base: HardforkConfig { azul: Some(110) },
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

        // Base Azul
        assert!(!cfg.is_first_base_azul_block(108));
        assert!(cfg.is_first_base_azul_block(110));
        assert!(!cfg.is_first_base_azul_block(112));
    }

    #[test]
    fn test_granite_channel_timeout() {
        let mut config = RollupConfig {
            channel_timeout: 100,
            hardforks: HardForkConfig { granite_time: Some(10), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(config.channel_timeout(0), 100);
        assert_eq!(config.channel_timeout(10), RollupConfig::GRANITE_CHANNEL_TIMEOUT);
        config.hardforks.granite_time = None;
        assert_eq!(config.channel_timeout(10), 100);
    }

    #[test]
    fn test_max_sequencer_drift() {
        let mut config = RollupConfig { max_sequencer_drift: 100, ..Default::default() };
        assert_eq!(config.max_sequencer_drift(0), 100);
        config.hardforks.fjord_time = Some(10);
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
            chain_op_config: FeeConfig::from_chain_id(0),
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
