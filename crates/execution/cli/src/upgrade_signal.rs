//! Execution-node upgrade signal schedule application.

use alloy_provider::RootProvider;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL, UpgradeSignalApplySummary,
    UpgradeSignalConfig, UpgradeSignalMetrics, UpgradeSignalMonitor, UpgradeSignalRefresher,
    UpgradeSignalSchedule, UpgradeSignalStateUpdate,
};
use jsonrpsee::{RpcModule, core::RpcResult, types::ErrorObject};
use reth_chainspec::EthChainSpec;
use reth_rpc_server_types::RethRpcModule;
use tracing::{debug, info, warn};
use url::Url;

/// Configuration for execution-node upgrade signal schedule reads.
#[derive(Debug, Clone)]
pub struct ExecutionUpgradeSignalConfig {
    /// Shared upgrade signal schedule read configuration.
    pub signal_config: UpgradeSignalConfig,
    /// L1 RPC URL used to read the upgrade signal contract.
    pub l1_rpc: Url,
}

/// Applies contract-backed upgrade signal schedules to execution node configuration.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionUpgradeSignal;

impl ExecutionUpgradeSignal {
    /// Applies the configured L1 upgrade signal to the chain spec before startup.
    pub async fn apply_initial_signal_to_chain_spec(
        config: &ExecutionUpgradeSignalConfig,
        chain_spec: &mut BaseChainSpec,
    ) -> eyre::Result<()> {
        let reader = AlloyUpgradeSignalReader::new(
            RootProvider::new_http(config.l1_rpc.clone()),
            config.signal_config.contract_address,
        );
        let schedule = match reader.read_schedule(&config.signal_config.hardfork_ids).await {
            Ok(schedule) => schedule,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors(&config.signal_config.hardfork_ids);
                return Err(error.into());
            }
        };
        UpgradeSignalMetrics::record_schedule(&schedule);
        for signal in &schedule.signals {
            info!(
                target: "upgrade_signal",
                hardfork_id = %signal.hardfork_id,
                activation_timestamp = signal.activation_timestamp,
                minimum_protocol_version = %signal.protocol_version,
                node_protocol_version = %config.signal_config.node_protocol_version,
                l1_block_number = signal.l1_block_number,
                "read dynamic upgrade signal for execution startup"
            );
        }
        config.signal_config.validate_schedule_protocol_versions(&schedule)?;

        Self::apply_schedule_to_chain_spec(chain_spec, &schedule)?;

        Ok(())
    }

    /// Applies a contract-backed hardfork activation schedule to an execution chain spec.
    pub fn apply_schedule_to_chain_spec(
        chain_spec: &mut BaseChainSpec,
        schedule: &UpgradeSignalSchedule,
    ) -> eyre::Result<usize> {
        let mut applied = 0;
        let mut cleared = 0;
        for signal in &schedule.signals {
            let Some(timestamp) = signal.positive_activation_timestamp() else {
                if !chain_spec.try_clear_hardfork_activation_timestamp(&signal.hardfork_id)? {
                    debug!(
                        target: "upgrade_signal",
                        hardfork_id = %signal.hardfork_id,
                        activation_timestamp = signal.activation_timestamp,
                        "ignored unsupported execution hardfork signal"
                    );
                    continue;
                }
                cleared += 1;
                info!(
                    target: "upgrade_signal",
                    hardfork_id = %signal.hardfork_id,
                    "cleared upgrade signal from execution chain spec"
                );
                continue;
            };
            if !chain_spec.try_set_hardfork_activation_timestamp(&signal.hardfork_id, timestamp)? {
                debug!(
                    target: "upgrade_signal",
                    hardfork_id = %signal.hardfork_id,
                    activation_timestamp = timestamp,
                    "ignored unsupported execution hardfork signal"
                );
                continue;
            }
            applied += 1;
            info!(
                target: "upgrade_signal",
                hardfork_id = %signal.hardfork_id,
                activation_timestamp = timestamp,
                "applied upgrade signal to execution chain spec"
            );
        }
        chain_spec.refresh_genesis_header();
        info!(
            target: "upgrade_signal",
            applied_hardforks = applied,
            cleared_hardforks = cleared,
            configured_hardforks = schedule.signals.len(),
            "applied upgrade signal schedule to execution chain spec"
        );

        Ok(applied)
    }

    /// Refreshes the runtime upgrade signal schedule for a running execution node.
    pub async fn refresh_runtime_upgrade_signal(
        refresher: &UpgradeSignalRefresher,
    ) -> RpcResult<UpgradeSignalApplySummary> {
        match refresher.refresh().await {
            Ok(summary) => {
                info!(
                    target: "upgrade_signal",
                    chain_id = summary.chain_id,
                    l1_block_number = ?summary.l1_block_number,
                    applied_hardforks = summary.applied_hardforks,
                    cleared_hardforks = summary.cleared_hardforks,
                    ignored_hardforks = summary.ignored_hardforks,
                    configured_hardforks = summary.configured_hardforks,
                    "refreshed execution runtime upgrade signal"
                );
                Ok(summary)
            }
            Err(error) => {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to refresh execution runtime upgrade signal"
                );
                Err(ErrorObject::owned(
                    -32003,
                    "failed to refresh upgrade signal",
                    Some(error.to_string()),
                ))
            }
        }
    }

    /// Registers the execution admin RPC method for runtime upgrade signal refreshes.
    pub fn register_runtime_refresh_rpc(
        ctx: &mut BaseRpcContext<'_>,
        config: ExecutionUpgradeSignalConfig,
    ) -> eyre::Result<()> {
        let chain_id = ctx.config().chain.chain().id();
        let refresher = UpgradeSignalRefresher::new(
            config.signal_config,
            RootProvider::new_http(config.l1_rpc),
            chain_id,
        );
        let mut module = RpcModule::new(refresher);
        module
            .register_async_method("admin_refreshUpgradeSignal", |_, refresher, _| async move {
                Self::refresh_runtime_upgrade_signal(&refresher).await
            })
            .map_err(|error| eyre::eyre!(error))?;
        ctx.modules.merge_if_module_configured(RethRpcModule::Admin, module)?;

        Ok(())
    }
}

/// Execution-node extension that records live L1 upgrade signal metrics only.
#[derive(Debug)]
pub struct ExecutionUpgradeSignalMetricsExtension {
    /// Extension configuration.
    pub config: ExecutionUpgradeSignalConfig,
}

impl ExecutionUpgradeSignalMetricsExtension {
    /// Creates a new execution upgrade signal metrics extension.
    pub const fn new(config: ExecutionUpgradeSignalConfig) -> Self {
        Self { config }
    }

    /// Polls L1 upgrade signal state and records metrics without mutating local config.
    pub async fn poll_l1_signal(
        monitor: &mut UpgradeSignalMonitor,
        reader: &AlloyUpgradeSignalReader,
        hardfork_ids: &[String],
    ) {
        match reader.read_schedule(hardfork_ids).await {
            Ok(schedule) => {
                let updates = monitor.update_schedule(schedule);
                let updated_hardforks = updates
                    .iter()
                    .filter(|update| matches!(update, UpgradeSignalStateUpdate::Changed))
                    .count();
                if updated_hardforks > 0 {
                    info!(
                        target: "upgrade_signal",
                        updated_hardforks,
                        "observed live L1 upgrade signal update"
                    );
                }
            }
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors(hardfork_ids);
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    hardfork_ids = ?hardfork_ids,
                    "failed to read live L1 upgrade signal metrics"
                );
            }
        }
    }
}

impl BaseNodeExtension for ExecutionUpgradeSignalMetricsExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let config = self.config;
        let rpc_config = config.clone();

        let hooks = hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            ExecutionUpgradeSignal::register_runtime_refresh_rpc(ctx, rpc_config)
        });

        hooks.add_node_started_hook(move |ctx| {
            let reader = AlloyUpgradeSignalReader::new(
                RootProvider::new_http(config.l1_rpc.clone()),
                config.signal_config.contract_address,
            );
            let hardfork_ids = config.signal_config.hardfork_ids;
            let mut monitor = UpgradeSignalMonitor::new(&hardfork_ids);
            let executor = ctx.task_executor;

            executor.spawn_with_graceful_shutdown_signal(|signal| {
                Box::pin(async move {
                    let mut interval = tokio::time::interval(DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL);
                    let mut signal = Box::pin(signal);

                    loop {
                        tokio::select! {
                            _ = &mut signal => break,
                            _ = interval.tick() => {
                                Self::poll_l1_signal(&mut monitor, &reader, &hardfork_ids).await;
                            }
                        }
                    }
                })
            });

            info!(target: "upgrade_signal", "execution upgrade signal metrics observer spawned");
            Ok(())
        })
    }
}

impl FromExtensionConfig for ExecutionUpgradeSignalMetricsExtension {
    type Config = ExecutionUpgradeSignalConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

#[cfg(test)]
mod tests {
    use base_common_chains::BaseUpgrade;
    use reth_chainspec::{ChainSpec, EthereumHardfork, ForkCondition};

    use super::*;

    fn schedule(signals: &[(&str, u64)]) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            signals
                .iter()
                .map(|(hardfork_id, activation_timestamp)| base_upgrade_signal::UpgradeSignal {
                    hardfork_id: hardfork_id.to_string(),
                    activation_timestamp: *activation_timestamp,
                    protocol_version: Default::default(),
                    l1_block_number: 1,
                })
                .collect(),
        )
    }

    #[test]
    fn applies_positive_schedule_to_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Shanghai, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Canyon, ForkCondition::Never);
        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("canyon", 40), ("azul", 42)]),
        )
        .unwrap();

        assert_eq!(applied, 2);
        assert_eq!(chain_spec.fork(EthereumHardfork::Shanghai), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(BaseUpgrade::Canyon), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Timestamp(42));
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Timestamp(42));
    }

    #[test]
    fn zero_signal_clears_existing_chain_spec_forks() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Shanghai, ForkCondition::Timestamp(40));
        chain_spec.set_fork(BaseUpgrade::Canyon, ForkCondition::Timestamp(40));
        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Timestamp(42));
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Timestamp(42));

        let applied = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("azul", 0)]),
        )
        .unwrap();

        assert_eq!(applied, 0);
        assert_eq!(chain_spec.fork(EthereumHardfork::Shanghai), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(BaseUpgrade::Canyon), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }

    #[test]
    fn ignores_unsupported_signal_for_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("delta", 42)]),
        )
        .unwrap();

        assert_eq!(applied, 0);
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }

    #[test]
    fn rejects_beryl_schedule_without_activation_admin() {
        let mut chain_spec = BaseChainSpec::from(ChainSpec::default());

        let error = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("beryl", 42)]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("missing activation admin address"));
        assert_eq!(chain_spec.fork(BaseUpgrade::Beryl), ForkCondition::Never);
    }
}
