use alloy_primitives::{Address, U256};
use base_common_genesis::HardForkConfig;
use url::Url;

use super::{
    UpgradeSignalBlockTag, UpgradeSignalConfig, UpgradeSignalConfigError, UpgradeSignalDefaults,
    UpgradeSignalMode,
};
use crate::state::UpgradeSignalSchedule;

/// CLI arguments shared by nodes that read the L1 upgrade signal contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalArgs {
    /// L1 upgrade signal contract or proxy address.
    #[arg(long = "upgrade-signal.contract", env = "BASE_NODE_UPGRADE_SIGNAL_CONTRACT")]
    pub contract_address: Option<Address>,

    /// Hardfork IDs to pass to the L1 upgrade signal contract.
    ///
    /// If omitted while the contract is configured, all timestamp-based Base rollup hardfork IDs
    /// are read.
    #[arg(
        long = "upgrade-signal.hardfork-id",
        env = "BASE_NODE_UPGRADE_SIGNAL_HARDFORK_ID",
        value_delimiter = ','
    )]
    pub hardfork_ids: Vec<String>,

    /// Hardfork IDs that are allowed to mutate the local schedule.
    ///
    /// If omitted, every read hardfork ID is eligible for application when the selected mode
    /// permits schedule mutation.
    #[arg(
        long = "upgrade-signal.apply-hardfork-id",
        env = "BASE_NODE_UPGRADE_SIGNAL_APPLY_HARDFORK_ID",
        value_delimiter = ','
    )]
    pub apply_hardfork_ids: Vec<String>,

    /// Upgrade signal application mode.
    #[arg(
        long = "upgrade-signal.mode",
        env = "BASE_NODE_UPGRADE_SIGNAL_MODE",
        value_enum,
        default_value_t = UpgradeSignalMode::MetricsOnly
    )]
    pub mode: UpgradeSignalMode,

    /// L1 block tag used to read the upgrade signal contract.
    #[arg(
        long = "upgrade-signal.l1-block-tag",
        env = "BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG",
        value_enum,
        default_value_t = UpgradeSignalBlockTag::Finalized
    )]
    pub l1_block_tag: UpgradeSignalBlockTag,
}

impl UpgradeSignalArgs {
    /// Builds a schedule read configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Result<Option<UpgradeSignalConfig>, UpgradeSignalConfigError> {
        let Some(contract_address) = self.contract_address else {
            if !self.hardfork_ids.is_empty() || !self.apply_hardfork_ids.is_empty() {
                return Err(UpgradeSignalConfigError::MissingContractAddress);
            }
            return Ok(None);
        };

        let hardfork_ids = Self::configured_hardfork_ids(&self.hardfork_ids)?;
        let apply_hardfork_ids =
            Self::configured_apply_hardfork_ids(&hardfork_ids, &self.apply_hardfork_ids)?;

        Ok(Some(UpgradeSignalConfig {
            contract_address,
            hardfork_ids,
            apply_hardfork_ids,
            mode: self.mode,
            l1_block_tag: self.l1_block_tag.block_number_or_tag(),
            node_protocol_version: U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION),
        }))
    }

    /// Returns the configured hardfork IDs, or the default contract-backed hardfork schedule.
    pub fn configured_hardfork_ids(
        hardfork_ids: &[String],
    ) -> Result<Vec<String>, UpgradeSignalConfigError> {
        let source = if hardfork_ids.is_empty() {
            HardForkConfig::CONTRACT_HARDFORK_IDS.to_vec()
        } else {
            hardfork_ids.iter().map(String::as_str).collect::<Vec<_>>()
        };
        let mut ids = Vec::new();
        for hardfork_id in source {
            let hardfork_id = hardfork_id.trim();
            if hardfork_id.is_empty() {
                return Err(UpgradeSignalConfigError::EmptyHardforkId);
            }
            if !ids.iter().any(|existing: &String| existing.eq_ignore_ascii_case(hardfork_id)) {
                ids.push(hardfork_id.to_string());
            }
        }

        Ok(ids)
    }

    /// Returns the configured apply hardfork IDs, or the read hardfork IDs when omitted.
    ///
    /// Every apply hardfork ID must also be a read hardfork ID, since only read signals can be
    /// applied. A non-subset apply ID is rejected rather than silently ignored.
    pub fn configured_apply_hardfork_ids(
        hardfork_ids: &[String],
        apply_hardfork_ids: &[String],
    ) -> Result<Vec<String>, UpgradeSignalConfigError> {
        if apply_hardfork_ids.is_empty() {
            return Ok(hardfork_ids.to_vec());
        }

        let apply_hardfork_ids = Self::configured_hardfork_ids(apply_hardfork_ids)?;
        for apply_hardfork_id in &apply_hardfork_ids {
            if !hardfork_ids.iter().any(|read_hardfork_id| {
                UpgradeSignalSchedule::hardfork_ids_match(read_hardfork_id, apply_hardfork_id)
            }) {
                return Err(UpgradeSignalConfigError::ApplyHardforkIdNotRead(
                    apply_hardfork_id.clone(),
                ));
            }
        }

        Ok(apply_hardfork_ids)
    }
}

/// TODO: Default this to the execution CLI's L1 RPC URL so users do not need to pass the same
/// endpoint twice. This likely requires refactoring how the upgrade signal args are wired into
/// the standalone execution CLI.
///
/// CLI argument for the L1 RPC endpoint used by standalone execution nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalL1RpcArgs {
    /// L1 execution RPC URL used to read the upgrade signal contract.
    #[arg(long = "upgrade-signal.l1-rpc", env = "BASE_NODE_UPGRADE_SIGNAL_L1_RPC")]
    pub upgrade_signal_l1_rpc: Option<Url>,
}
