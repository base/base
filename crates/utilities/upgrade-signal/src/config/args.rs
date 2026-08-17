use alloy_primitives::Address;
use base_common_genesis::UpgradeActivationSink;
use url::Url;

use super::{UpgradeSignalBlockTag, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalMode};

/// CLI arguments shared by nodes that read the L1 upgrade signal contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalArgs {
    /// L1 upgrade signal contract or proxy address.
    #[arg(long = "upgrade-signal.contract", env = "BASE_NODE_UPGRADE_SIGNAL_CONTRACT")]
    pub contract_address: Option<Address>,

    /// Upgrade signal application mode.
    #[arg(
        long = "upgrade-signal.mode",
        env = "BASE_NODE_UPGRADE_SIGNAL_MODE",
        value_enum,
        default_value_t = UpgradeSignalMode::default()
    )]
    pub mode: UpgradeSignalMode,

    /// L1 block tag used to read the upgrade signal contract. Also selects the interval between
    /// live contract reads: 15m for finalized, 6m24s for safe, 12s for latest.
    #[arg(
        long = "upgrade-signal.l1-block-tag",
        env = "BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG",
        value_enum,
        default_value_t = UpgradeSignalBlockTag::default()
    )]
    pub l1_block_tag: UpgradeSignalBlockTag,
}

impl UpgradeSignalArgs {
    /// Builds a schedule read configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Option<UpgradeSignalConfig> {
        let contract_address = self.contract_address?;

        Some(UpgradeSignalConfig {
            contract_address,
            mode: self.mode,
            l1_block_tag: self.l1_block_tag,
            node_protocol_version: UpgradeSignalDefaults::node_protocol_version(),
            request_timeout: UpgradeSignalDefaults::REQUEST_TIMEOUT,
        })
    }

    /// Returns startup config when the selected mode applies the signal before startup.
    pub fn startup_config(
        &self,
        l1_rpc_args: &UpgradeSignalL1RpcArgs,
    ) -> eyre::Result<Option<UpgradeSignalStartupConfig>> {
        let Some(signal_config) = self.config() else {
            return Ok(None);
        };
        if !signal_config.mode.applies_at_startup() {
            return Ok(None);
        }

        let l1_rpc = l1_rpc_args.required_l1_rpc()?;
        Ok(Some(UpgradeSignalStartupConfig { signal_config, l1_rpc }))
    }

    /// Applies one startup read to both execution and consensus schedules when startup mode applies.
    pub async fn apply_startup_to_sinks<EL, CL>(
        &self,
        l1_rpc_args: &UpgradeSignalL1RpcArgs,
        log_context: &'static str,
        chain_id: u64,
        execution_sink: &mut EL,
        consensus_sink: &mut CL,
    ) -> eyre::Result<()>
    where
        EL: UpgradeActivationSink + Clone,
        EL::Error: std::error::Error + Send + Sync + 'static,
        CL: UpgradeActivationSink + Clone,
        CL::Error: std::error::Error + Send + Sync + 'static,
    {
        if let Some(startup_config) = self.startup_config(l1_rpc_args)? {
            startup_config
                .apply_to_sinks(log_context, chain_id, execution_sink, consensus_sink)
                .await?;
        }

        Ok(())
    }
}

/// Startup upgrade signal config with a resolved L1 RPC.
#[derive(Debug, Clone)]
pub struct UpgradeSignalStartupConfig {
    /// Schedule read configuration.
    pub signal_config: UpgradeSignalConfig,
    /// L1 RPC used to read the upgrade signal contract.
    pub l1_rpc: Url,
}

impl UpgradeSignalStartupConfig {
    /// Applies one startup read to both execution and consensus schedules.
    pub async fn apply_to_sinks<EL, CL>(
        self,
        log_context: &'static str,
        chain_id: u64,
        execution_sink: &mut EL,
        consensus_sink: &mut CL,
    ) -> eyre::Result<()>
    where
        EL: UpgradeActivationSink + Clone,
        EL::Error: std::error::Error + Send + Sync + 'static,
        CL: UpgradeActivationSink + Clone,
        CL::Error: std::error::Error + Send + Sync + 'static,
    {
        self.signal_config
            .apply_startup_to_sinks(
                self.l1_rpc,
                log_context,
                chain_id,
                execution_sink,
                consensus_sink,
            )
            .await
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

    /// Returns the configured L1 RPC, or an internal-error if it was never set.
    pub fn required_l1_rpc(&self) -> eyre::Result<Url> {
        self.upgrade_signal_l1_rpc
            .clone()
            .ok_or_else(|| eyre::eyre!("upgrade signal L1 RPC not derived; this is a bug"))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn disabled_when_no_contract() {
        let args = UpgradeSignalArgs::default();

        assert_eq!(args.config(), None);
    }

    #[test]
    fn enabled_for_contract() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };

        let config = args.config().unwrap();

        assert_eq!(config.mode, UpgradeSignalMode::MetricsOnly);
    }

    #[test]
    fn maps_configured_block_tag() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            l1_block_tag: UpgradeSignalBlockTag::Latest,
            ..Default::default()
        };

        assert_eq!(args.config().unwrap().l1_block_tag, UpgradeSignalBlockTag::Latest);
    }

    #[test]
    fn builds_enabled_config() {
        let contract = address!("0000000000000000000000000000000000000001");
        let args = UpgradeSignalArgs {
            contract_address: Some(contract),
            mode: UpgradeSignalMode::StartupApply,
            ..Default::default()
        };

        let config = args.config().unwrap();

        assert_eq!(config.contract_address, contract);
        assert_eq!(config.mode, UpgradeSignalMode::StartupApply);
        assert_eq!(config.node_protocol_version, UpgradeSignalDefaults::node_protocol_version());
    }

    #[test]
    fn startup_config_is_none_when_mode_does_not_apply_at_startup() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };

        assert!(args.startup_config(&UpgradeSignalL1RpcArgs::default()).unwrap().is_none());
    }

    #[test]
    fn startup_config_requires_l1_rpc_when_mode_applies_at_startup() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            mode: UpgradeSignalMode::StartupApply,
            ..Default::default()
        };

        let error = args.startup_config(&UpgradeSignalL1RpcArgs::default()).unwrap_err();

        assert_eq!(error.to_string(), "upgrade signal L1 RPC not derived; this is a bug");
    }

    #[test]
    fn startup_config_returns_resolved_config_when_mode_applies_at_startup() {
        let contract_address = address!("0000000000000000000000000000000000000001");
        let l1_rpc = Url::parse("http://l1:8545").unwrap();
        let args = UpgradeSignalArgs {
            contract_address: Some(contract_address),
            mode: UpgradeSignalMode::StartupApply,
            ..Default::default()
        };
        let l1_rpc_args = UpgradeSignalL1RpcArgs { upgrade_signal_l1_rpc: Some(l1_rpc.clone()) };

        let config = args.startup_config(&l1_rpc_args).unwrap().unwrap();

        assert_eq!(config.signal_config.contract_address, contract_address);
        assert_eq!(config.l1_rpc, l1_rpc);
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
}
