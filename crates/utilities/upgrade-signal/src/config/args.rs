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

    /// Upgrade IDs to pass to the L1 upgrade signal contract.
    ///
    /// If omitted while the contract is configured, all contract-backed Base upgrade IDs are
    /// read.
    #[arg(
        long = "upgrade-signal.upgrade-id",
        env = "BASE_NODE_UPGRADE_SIGNAL_UPGRADE_ID",
        value_delimiter = ','
    )]
    pub upgrade_ids: Vec<String>,

    /// Upgrade IDs that are allowed to mutate the local schedule.
    ///
    /// If omitted, every read upgrade ID is eligible for application when the selected mode
    /// permits schedule mutation.
    #[arg(
        long = "upgrade-signal.apply-upgrade-id",
        env = "BASE_NODE_UPGRADE_SIGNAL_APPLY_UPGRADE_ID",
        value_delimiter = ','
    )]
    pub apply_upgrade_ids: Vec<String>,

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

    /// Enable execution-layer live upgrade-signal metrics.
    #[arg(long = "upgrade-signal.el-metrics", env = "BASE_NODE_UPGRADE_SIGNAL_EL_METRICS")]
    pub el_metrics_enabled: bool,

    /// Enable consensus-layer live upgrade-signal metrics.
    #[arg(long = "upgrade-signal.cl-metrics", env = "BASE_NODE_UPGRADE_SIGNAL_CL_METRICS")]
    pub cl_metrics_enabled: bool,
}

impl UpgradeSignalArgs {
    /// Builds a schedule read configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Result<Option<UpgradeSignalConfig>, UpgradeSignalConfigError> {
        let Some(contract_address) = self.contract_address else {
            if !self.upgrade_ids.is_empty()
                || !self.apply_upgrade_ids.is_empty()
                || self.el_metrics_enabled
                || self.cl_metrics_enabled
            {
                return Err(UpgradeSignalConfigError::MissingContractAddress);
            }
            return Ok(None);
        };

        let upgrade_ids = Self::configured_upgrade_ids(&self.upgrade_ids)?;
        let apply_upgrade_ids =
            Self::configured_apply_upgrade_ids(&upgrade_ids, &self.apply_upgrade_ids)?;

        Ok(Some(UpgradeSignalConfig {
            contract_address,
            upgrade_ids,
            apply_upgrade_ids,
            mode: self.mode,
            l1_block_tag: self.l1_block_tag.block_number_or_tag(),
            node_protocol_version: U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION),
            el_metrics_enabled: self.el_metrics_enabled,
            cl_metrics_enabled: self.cl_metrics_enabled,
        }))
    }

    /// Returns the configured upgrade IDs, or the default contract-backed upgrade schedule.
    pub fn configured_upgrade_ids(
        upgrade_ids: &[String],
    ) -> Result<Vec<BaseUpgrade>, UpgradeSignalConfigError> {
        if upgrade_ids.is_empty() {
            return Ok(BaseUpgrade::CONTRACT_VARIANTS.to_vec());
        }

        let source = upgrade_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let mut ids = Vec::new();
        for upgrade_id in source {
            let upgrade_id = upgrade_id.trim();
            if upgrade_id.is_empty() {
                return Err(UpgradeSignalConfigError::EmptyUpgradeId);
            }
            let upgrade_id = BaseUpgrade::from_contract_fork_name(upgrade_id).ok_or_else(|| {
                UpgradeSignalConfigError::UnknownUpgradeId(upgrade_id.to_string())
            })?;
            if !ids.contains(&upgrade_id) {
                ids.push(upgrade_id);
            }
        }

        Ok(ids)
    }

    /// Returns the configured apply upgrade IDs, or the read upgrade IDs when omitted.
    ///
    /// Every apply upgrade ID must also be a read upgrade ID, since only read signals can be
    /// applied. A non-subset apply ID is rejected rather than silently ignored.
    pub fn configured_apply_upgrade_ids(
        upgrade_ids: &[BaseUpgrade],
        apply_upgrade_ids: &[String],
    ) -> Result<Vec<BaseUpgrade>, UpgradeSignalConfigError> {
        if apply_upgrade_ids.is_empty() {
            return Ok(upgrade_ids.to_vec());
        }

        let apply_upgrade_ids = Self::configured_upgrade_ids(apply_upgrade_ids)?;
        for apply_upgrade_id in &apply_upgrade_ids {
            if !upgrade_ids.contains(apply_upgrade_id) {
                return Err(UpgradeSignalConfigError::ApplyUpgradeIdNotRead(
                    apply_upgrade_id.contract_id().to_string(),
                ));
            }
        }

        Ok(apply_upgrade_ids)
    }
}

/// CLI argument for the L1 RPC endpoint used by execution upgrade-signal polling.
///
/// Integrated callers may default this from the consensus L1 RPC so both services read from the
/// same L1 endpoint by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalL1RpcArgs {
    /// L1 execution RPC URL used to read the upgrade signal contract.
    #[arg(long = "upgrade-signal.l1-rpc", env = "BASE_NODE_UPGRADE_SIGNAL_L1_RPC")]
    pub upgrade_signal_l1_rpc: Option<Url>,
}

impl UpgradeSignalL1RpcArgs {
    /// Defaults the execution upgrade-signal L1 RPC from another service's L1 RPC when unset.
    pub fn apply_default_from(&mut self, l1_rpc: &Url) {
        self.upgrade_signal_l1_rpc.get_or_insert_with(|| l1_rpc.clone());
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use alloy_rpc_types_eth::BlockNumberOrTag;
    use base_common_genesis::BaseUpgrade;
    use clap::Parser;

    use super::*;
    use crate::UpgradeSignalMetricLayer;

    #[derive(Parser)]
    struct CommandParser {
        #[command(flatten)]
        args: UpgradeSignalArgs,
    }

    #[test]
    fn disabled_when_no_contract_or_upgrade_id() {
        let args = UpgradeSignalArgs::default();

        assert_eq!(args.config().unwrap(), None);
    }

    #[test]
    fn uses_default_ids_for_contract_without_upgrade_id() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.upgrade_ids, BaseUpgrade::CONTRACT_VARIANTS.to_vec());
        assert_eq!(config.apply_upgrade_ids, BaseUpgrade::CONTRACT_VARIANTS.to_vec());
        assert_eq!(config.mode, UpgradeSignalMode::MetricsOnly);
        assert!(!config.metrics_enabled(UpgradeSignalMetricLayer::Execution));
        assert!(!config.metrics_enabled(UpgradeSignalMetricLayer::Consensus));
    }

    #[test]
    fn rejects_upgrade_id_without_contract() {
        let args =
            UpgradeSignalArgs { upgrade_ids: vec!["azul".to_string()], ..Default::default() };

        assert!(matches!(
            args.config().unwrap_err(),
            UpgradeSignalConfigError::MissingContractAddress
        ));
    }

    #[test]
    fn rejects_metrics_without_contract() {
        let args = UpgradeSignalArgs { el_metrics_enabled: true, ..Default::default() };

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
            upgrade_ids: vec!["azul".to_string()],
            mode: UpgradeSignalMode::StartupApply,
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.contract_address, contract);
        assert_eq!(config.upgrade_ids, [BaseUpgrade::Azul]);
        assert_eq!(config.apply_upgrade_ids, [BaseUpgrade::Azul]);
        assert_eq!(config.mode, UpgradeSignalMode::StartupApply);
        assert!(!config.metrics_enabled(UpgradeSignalMetricLayer::Execution));
        assert!(!config.metrics_enabled(UpgradeSignalMetricLayer::Consensus));
        assert_eq!(
            config.node_protocol_version,
            U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn enables_layer_metrics_explicitly() {
        let args = CommandParser::parse_from([
            "test",
            "--upgrade-signal.contract",
            "0x0000000000000000000000000000000000000001",
            "--upgrade-signal.el-metrics",
            "--upgrade-signal.cl-metrics",
        ])
        .args;

        let config = args.config().unwrap().unwrap();

        assert!(config.metrics_enabled(UpgradeSignalMetricLayer::Execution));
        assert!(config.metrics_enabled(UpgradeSignalMetricLayer::Consensus));
    }

    #[test]
    fn defaults_execution_l1_rpc_from_shared_l1_rpc() {
        let mut args = UpgradeSignalL1RpcArgs::default();
        let l1_rpc = Url::parse("http://localhost:8545").unwrap();

        args.apply_default_from(&l1_rpc);

        assert_eq!(args.upgrade_signal_l1_rpc.as_ref().map(Url::as_str), Some(l1_rpc.as_str()));
    }

    #[test]
    fn preserves_explicit_execution_l1_rpc_when_defaulting() {
        let explicit_l1_rpc = Url::parse("http://finalized-l1:8545").unwrap();
        let mut args =
            UpgradeSignalL1RpcArgs { upgrade_signal_l1_rpc: Some(explicit_l1_rpc.clone()) };

        args.apply_default_from(&Url::parse("http://localhost:8545").unwrap());

        assert_eq!(
            args.upgrade_signal_l1_rpc.as_ref().map(Url::as_str),
            Some(explicit_l1_rpc.as_str())
        );
    }

    #[test]
    fn uses_explicit_apply_ids() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            upgrade_ids: vec!["azul".to_string(), "beryl".to_string()],
            apply_upgrade_ids: vec!["beryl".to_string()],
            mode: UpgradeSignalMode::RuntimeAdmin,
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.upgrade_ids, [BaseUpgrade::Azul, BaseUpgrade::Beryl]);
        assert_eq!(config.apply_upgrade_ids, [BaseUpgrade::Beryl]);
        assert_eq!(config.mode, UpgradeSignalMode::RuntimeAdmin);
    }

    #[test]
    fn rejects_apply_id_not_in_read_ids() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            upgrade_ids: vec!["azul".to_string()],
            apply_upgrade_ids: vec!["beryl".to_string()],
            ..Default::default()
        };

        assert!(matches!(
            args.config().unwrap_err(),
            UpgradeSignalConfigError::ApplyUpgradeIdNotRead(_)
        ));
    }
}
