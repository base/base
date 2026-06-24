//! Execution-node upgrade signal schedule application.

use alloy_provider::RootProvider;
use base_common_genesis::BaseUpgrade;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalApplySummary, UpgradeSignalConfig,
    UpgradeSignalDefaults, UpgradeSignalMetricLayer, UpgradeSignalMonitor, UpgradeSignalRefresher,
    UpgradeSignalRuntimeApplier, UpgradeSignalRuntimeValidation, UpgradeSignalSchedule,
};
use jsonrpsee::{RpcModule, core::RpcResult, types::ErrorObject};
use reth_chainspec::EthChainSpec;
use reth_rpc_server_types::RethRpcModule;
use tracing::{info, warn};
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
        let reader = config.signal_config.reader(RootProvider::new_http(config.l1_rpc.clone()));
        let schedule = config
            .signal_config
            .read_validated_schedule(
                &reader,
                "execution startup",
                &[UpgradeSignalMetricLayer::Execution],
            )
            .await?;

        Self::validate_runtime_schedule_for_chain_spec(chain_spec, &schedule)?;
        Self::apply_schedule_to_chain_spec(chain_spec, &schedule)?;

        Ok(())
    }

    /// Applies a contract-backed upgrade activation schedule to an execution chain spec.
    pub fn apply_schedule_to_chain_spec(
        chain_spec: &mut BaseChainSpec,
        schedule: &UpgradeSignalSchedule,
    ) -> eyre::Result<usize> {
        let chain_id = chain_spec.chain().id();
        let summary =
            UpgradeSignalRuntimeApplier::apply_schedule_to_sink(chain_id, schedule, chain_spec)?;
        summary.log("execution chain spec");

        Ok(summary.applied_upgrades)
    }

    /// Validates that a runtime schedule can be applied to this execution chain spec.
    pub fn validate_runtime_schedule_for_chain_spec(
        chain_spec: &BaseChainSpec,
        schedule: &UpgradeSignalSchedule,
    ) -> eyre::Result<()> {
        UpgradeSignalRuntimeValidation::with_activation_admin_address(
            chain_spec.activation_admin_address,
        )
        .validate_schedule(chain_spec.chain().id(), schedule)?;

        Ok(())
    }

    /// Refreshes the runtime upgrade signal schedule for a running execution node.
    pub async fn refresh_runtime_upgrade_signal(
        refresher: &ExecutionUpgradeSignalRuntimeRefresher,
    ) -> RpcResult<UpgradeSignalApplySummary> {
        match refresher.refresher.read_validated_schedule().await {
            Ok(schedule) => {
                if let Err(error) =
                    Self::validate_runtime_schedule_for_chain_spec(&refresher.chain_spec, &schedule)
                {
                    warn!(
                        target: "upgrade_signal",
                        error = %error,
                        "failed to validate execution runtime upgrade signal"
                    );
                    return Err(ErrorObject::owned(
                        -32005,
                        "failed to validate upgrade signal",
                        None::<()>,
                    ));
                }
                let summary = UpgradeSignalRuntimeApplier::apply_schedule(
                    refresher.refresher.chain_id,
                    &schedule,
                );
                info!(
                    target: "upgrade_signal",
                    chain_id = summary.chain_id,
                    l1_block_number = ?summary.l1_block_number,
                    applied_upgrades = summary.applied_upgrades,
                    cleared_upgrades = summary.cleared_upgrades,
                    ignored_upgrades = summary.ignored_upgrades,
                    configured_upgrades = summary.configured_upgrades,
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
                Err(ErrorObject::owned(-32003, "failed to refresh upgrade signal", None::<()>))
            }
        }
    }

    /// Registers the execution admin RPC method for runtime upgrade signal refreshes.
    pub fn register_runtime_refresh_rpc(
        ctx: &mut BaseRpcContext<'_>,
        config: ExecutionUpgradeSignalConfig,
    ) -> eyre::Result<()> {
        if !config.signal_config.mode.allows_runtime_admin() {
            return Ok(());
        }

        let chain_id = ctx.config().chain.chain().id();
        // Execution validates each refresh against the live chain spec in
        // `refresh_runtime_upgrade_signal`, so the shared refresher itself stays unvalidated.
        let refresher = ExecutionUpgradeSignalRuntimeRefresher::new(
            UpgradeSignalRefresher::new(
                config.signal_config,
                RootProvider::new_http(config.l1_rpc),
                chain_id,
                UpgradeSignalRuntimeValidation::disabled(),
                UpgradeSignalMetricLayer::Execution,
            ),
            ctx.config().chain.as_ref().clone(),
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

/// Execution runtime upgrade signal refresher with execution-specific validation context.
#[derive(Debug, Clone)]
pub struct ExecutionUpgradeSignalRuntimeRefresher {
    /// Shared runtime refresher.
    pub refresher: UpgradeSignalRefresher,
    /// Execution chain spec used for runtime schedule validation.
    pub chain_spec: BaseChainSpec,
}

impl ExecutionUpgradeSignalRuntimeRefresher {
    /// Creates an execution runtime upgrade signal refresher.
    pub const fn new(refresher: UpgradeSignalRefresher, chain_spec: BaseChainSpec) -> Self {
        Self { refresher, chain_spec }
    }
}

/// Execution-node extension that registers runtime admin refresh and optional live metrics.
#[derive(Debug)]
pub struct ExecutionUpgradeSignalRuntimeExtension {
    /// Extension configuration.
    pub config: ExecutionUpgradeSignalConfig,
}

impl ExecutionUpgradeSignalRuntimeExtension {
    /// Creates a new execution upgrade signal runtime extension.
    pub const fn new(config: ExecutionUpgradeSignalConfig) -> Self {
        Self { config }
    }

    /// Polls L1 upgrade signal state and records metrics without mutating local config.
    pub async fn poll_l1_signal(
        monitor: &mut UpgradeSignalMonitor,
        reader: &AlloyUpgradeSignalReader,
        upgrade_ids: &[BaseUpgrade],
    ) {
        let updated_upgrades = monitor.poll(reader, upgrade_ids).await;
        if updated_upgrades > 0 {
            info!(
                target: "upgrade_signal",
                updated_upgrades,
                "observed live L1 upgrade signal update"
            );
        }
    }
}

impl BaseNodeExtension for ExecutionUpgradeSignalRuntimeExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let config = self.config;

        let hooks = if config.signal_config.mode.allows_runtime_admin() {
            let rpc_config = config.clone();
            hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
                ExecutionUpgradeSignal::register_runtime_refresh_rpc(ctx, rpc_config)
            })
        } else {
            hooks
        };

        hooks.add_node_started_hook(move |ctx| {
            let reader = config
                .signal_config
                .reader(RootProvider::new_http(config.l1_rpc.clone()));
            let upgrade_ids = config.signal_config.upgrade_ids;
            let mut monitor =
                UpgradeSignalMonitor::new(UpgradeSignalMetricLayer::Execution, &upgrade_ids);
            let executor = ctx.task_executor;

            executor.spawn_with_graceful_shutdown_signal(|signal| {
                Box::pin(async move {
                    let mut interval = tokio::time::interval(UpgradeSignalDefaults::POLL_INTERVAL);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    let mut signal = Box::pin(signal);

                    loop {
                        tokio::select! {
                            _ = &mut signal => break,
                            _ = interval.tick() => {
                                tokio::select! {
                                    _ = &mut signal => break,
                                    _ = Self::poll_l1_signal(&mut monitor, &reader, &upgrade_ids) => {}
                                }
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

impl FromExtensionConfig for ExecutionUpgradeSignalRuntimeExtension {
    type Config = ExecutionUpgradeSignalConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;
    use reth_chainspec::{ChainSpec, EthereumHardfork, ForkCondition};

    use super::*;

    fn schedule(signals: &[(BaseUpgrade, u64)]) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            signals
                .iter()
                .map(|(upgrade_id, activation_timestamp)| base_upgrade_signal::UpgradeSignal {
                    upgrade_id: *upgrade_id,
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
            &schedule(&[(BaseUpgrade::Canyon, 40), (BaseUpgrade::Azul, 42)]),
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
            &schedule(&[(BaseUpgrade::Azul, 0)]),
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
            &schedule(&[(BaseUpgrade::Delta, 42)]),
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
            &schedule(&[(BaseUpgrade::Beryl, 42)]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("missing activation admin address"));
        assert_eq!(chain_spec.fork(BaseUpgrade::Beryl), ForkCondition::Never);
    }
}
