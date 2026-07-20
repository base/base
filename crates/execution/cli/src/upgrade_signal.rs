//! Execution-node upgrade signal schedule application.

use alloy_provider::RootProvider;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    UpgradeSignalApplySummary, UpgradeSignalConfig, UpgradeSignalDefaults,
    UpgradeSignalMetricLayer, UpgradeSignalMonitor, UpgradeSignalRefresher,
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
        match refresher.refresher.read_schedule().await {
            Ok(schedule) => refresher.apply(&schedule).map_err(|error| {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to validate execution runtime upgrade signal"
                );
                ErrorObject::owned(-32005, "failed to validate upgrade signal", None::<()>)
            }),
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
        // Chain-spec validation happens per-apply in the execution wrapper, so the shared
        // refresher itself carries no runtime validation.
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

    /// Validates an already-read schedule against the execution chain spec and applies it.
    pub fn apply(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> eyre::Result<UpgradeSignalApplySummary> {
        ExecutionUpgradeSignal::validate_runtime_schedule_for_chain_spec(
            &self.chain_spec,
            schedule,
        )?;
        Ok(self.refresher.apply(schedule)?)
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
            let l1_provider = RootProvider::new_http(config.l1_rpc.clone());
            let reader = config.signal_config.reader(l1_provider.clone());
            // Live updates are validated against the startup chain spec and re-applied
            // automatically, matching the manual `admin_refreshUpgradeSignal` path.
            let auto_refresher = config.signal_config.mode.allows_runtime_admin().then(|| {
                let chain_spec = ctx.chain_spec();
                ExecutionUpgradeSignalRuntimeRefresher::new(
                    UpgradeSignalRefresher::new(
                        config.signal_config.clone(),
                        l1_provider,
                        chain_spec.chain().id(),
                        UpgradeSignalRuntimeValidation::disabled(),
                        UpgradeSignalMetricLayer::Execution,
                    ),
                    chain_spec.as_ref().clone(),
                )
            });
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
                                    polled = monitor.poll(&reader, &upgrade_ids) => {
                                        if let Some(refresher) = &auto_refresher
                                            && let Some(schedule) = polled
                                            && let Err(error) = refresher.apply(&schedule)
                                        {
                                            warn!(
                                                target: "upgrade_signal",
                                                error = %error,
                                                "failed to auto-apply live upgrade signal update"
                                            );
                                        }
                                    }
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
    use alloy_primitives::Address;
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation};
    use reth_chainspec::{ChainSpec, EthereumHardfork, ForkCondition};

    use super::*;

    fn runtime_refresher(
        chain_id: u64,
        chain_spec: BaseChainSpec,
    ) -> ExecutionUpgradeSignalRuntimeRefresher {
        ExecutionUpgradeSignalRuntimeRefresher::new(
            UpgradeSignalRefresher::new(
                UpgradeSignalConfig::new(Address::ZERO, BaseUpgrade::Azul),
                RootProvider::new_http("http://127.0.0.1:1".parse().unwrap()),
                chain_id,
                UpgradeSignalRuntimeValidation::disabled(),
                UpgradeSignalMetricLayer::Execution,
            ),
            chain_spec,
        )
    }

    fn versioned_schedule(
        upgrade_id: BaseUpgrade,
        activation_timestamp: u64,
    ) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(vec![base_upgrade_signal::UpgradeSignal {
            upgrade_id,
            activation_timestamp,
            protocol_version: UpgradeSignalDefaults::node_protocol_version(),
            l1_block_number: 1,
        }])
    }

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

    #[test]
    fn apply_applies_validated_schedule_to_registry() {
        let chain_id = 9_100_004;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let summary = runtime_refresher(chain_id, BaseChainSpec::devnet())
            .apply(&versioned_schedule(BaseUpgrade::Azul, 42))
            .unwrap();

        assert_eq!(summary.applied_upgrades, 1);
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn apply_rejects_invalid_schedule_without_mutating_registry() {
        let chain_id = 9_100_005;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let error = runtime_refresher(chain_id, BaseChainSpec::from(ChainSpec::default()))
            .apply(&versioned_schedule(BaseUpgrade::Beryl, 42))
            .unwrap_err();

        assert!(error.to_string().contains("missing activation admin address"));
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl), None);
    }
}
