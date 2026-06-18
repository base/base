use alloy_primitives::{Address, U256};
use base_common_genesis::BaseUpgrade;
use url::Url;

use super::{
    UpgradeSignalBlockTag, UpgradeSignalConfig, UpgradeSignalConfigError, UpgradeSignalDefaults,
    UpgradeSignalMode,
};

/// CLI arguments shared by nodes that read the L1 upgrade signal contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalArgs {
    /// L1 upgrade signal contract or proxy address.
    #[arg(long = "upgrade-signal.contract", env = "BASE_NODE_UPGRADE_SIGNAL_CONTRACT")]
    pub contract_address: Option<Address>,

    /// Hardfork IDs to pass to the L1 upgrade signal contract.
    ///
    /// If omitted while the contract is configured, all contract-backed Base hardfork IDs are
    /// read.
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
    ) -> Result<Vec<BaseUpgrade>, UpgradeSignalConfigError> {
        if hardfork_ids.is_empty() {
            return Ok(BaseUpgrade::CONTRACT_VARIANTS.to_vec());
        }

        let source = hardfork_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let mut ids = Vec::new();
        for hardfork_id in source {
            let hardfork_id = hardfork_id.trim();
            if hardfork_id.is_empty() {
                return Err(UpgradeSignalConfigError::EmptyHardforkId);
            }
            let hardfork_id =
                BaseUpgrade::from_contract_fork_name(hardfork_id).ok_or_else(|| {
                    UpgradeSignalConfigError::UnknownHardforkId(hardfork_id.to_string())
                })?;
            if !ids.contains(&hardfork_id) {
                ids.push(hardfork_id);
            }
        }

        Ok(ids)
    }

    /// Returns the configured apply hardfork IDs, or the read hardfork IDs when omitted.
    ///
    /// Every apply hardfork ID must also be a read hardfork ID, since only read signals can be
    /// applied. A non-subset apply ID is rejected rather than silently ignored.
    pub fn configured_apply_hardfork_ids(
        hardfork_ids: &[BaseUpgrade],
        apply_hardfork_ids: &[String],
    ) -> Result<Vec<BaseUpgrade>, UpgradeSignalConfigError> {
        if apply_hardfork_ids.is_empty() {
            return Ok(hardfork_ids.to_vec());
        }

        let apply_hardfork_ids = Self::configured_hardfork_ids(apply_hardfork_ids)?;
        for apply_hardfork_id in &apply_hardfork_ids {
            if !hardfork_ids.contains(apply_hardfork_id) {
                return Err(UpgradeSignalConfigError::ApplyHardforkIdNotRead(
                    apply_hardfork_id.contract_id().to_string(),
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

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use alloy_rpc_types_eth::BlockNumberOrTag;
    use base_common_genesis::BaseUpgrade;

    use super::*;

    #[test]
    fn disabled_when_no_contract_or_hardfork_id() {
        let args = UpgradeSignalArgs::default();

        assert_eq!(args.config().unwrap(), None);
    }

    #[test]
    fn uses_default_ids_for_contract_without_hardfork_id() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.hardfork_ids, BaseUpgrade::CONTRACT_VARIANTS.to_vec());
        assert_eq!(config.apply_hardfork_ids, BaseUpgrade::CONTRACT_VARIANTS.to_vec());
        assert_eq!(config.mode, UpgradeSignalMode::MetricsOnly);
    }

    #[test]
    fn rejects_hardfork_id_without_contract() {
        let args =
            UpgradeSignalArgs { hardfork_ids: vec!["azul".to_string()], ..Default::default() };

        assert!(matches!(
            args.config().unwrap_err(),
            UpgradeSignalConfigError::MissingContractAddress
        ));
    }

    #[test]
    fn maps_configured_block_tag() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            l1_block_tag: UpgradeSignalBlockTag::Latest,
            ..Default::default()
        };

        assert_eq!(args.config().unwrap().unwrap().l1_block_tag, BlockNumberOrTag::Latest);
    }

    #[test]
    fn builds_enabled_config() {
        let contract = address!("0000000000000000000000000000000000000001");
        let args = UpgradeSignalArgs {
            contract_address: Some(contract),
            hardfork_ids: vec!["azul".to_string()],
            mode: UpgradeSignalMode::StartupApply,
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.contract_address, contract);
        assert_eq!(config.hardfork_ids, [BaseUpgrade::Azul]);
        assert_eq!(config.apply_hardfork_ids, [BaseUpgrade::Azul]);
        assert_eq!(config.mode, UpgradeSignalMode::StartupApply);
        assert_eq!(
            config.node_protocol_version,
            U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn uses_explicit_apply_ids() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            hardfork_ids: vec!["azul".to_string(), "beryl".to_string()],
            apply_hardfork_ids: vec!["beryl".to_string()],
            mode: UpgradeSignalMode::RuntimeAdmin,
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.hardfork_ids, [BaseUpgrade::Azul, BaseUpgrade::Beryl]);
        assert_eq!(config.apply_hardfork_ids, [BaseUpgrade::Beryl]);
        assert_eq!(config.mode, UpgradeSignalMode::RuntimeAdmin);
    }

    #[test]
    fn rejects_apply_id_not_in_read_ids() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            hardfork_ids: vec!["azul".to_string()],
            apply_hardfork_ids: vec!["beryl".to_string()],
            ..Default::default()
        };

        assert!(matches!(
            args.config().unwrap_err(),
            UpgradeSignalConfigError::ApplyHardforkIdNotRead(_)
        ));
    }
}
