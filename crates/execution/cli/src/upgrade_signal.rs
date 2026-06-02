//! Execution-node upgrade signal extension.

use alloy_provider::RootProvider;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL, UpgradeSignalConfig,
    UpgradeSignalError, UpgradeSignalMonitor, UpgradeSignalReader, UpgradeSignalSchedule,
};
use reth_chain_state::CanonStateSubscriptions;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::{debug, info, warn};
use url::Url;

/// Configuration for the execution-node upgrade signal extension.
#[derive(Debug, Clone)]
pub struct ExecutionUpgradeSignalConfig {
    /// Shared upgrade signal observer configuration.
    pub signal_config: UpgradeSignalConfig,
    /// L1 RPC URL used to read the upgrade signal contract.
    pub l1_rpc: Url,
}

/// Execution-node extension that observes L1 upgrade signals and canonical L2 timestamps.
#[derive(Debug)]
pub struct ExecutionUpgradeSignalExtension {
    /// Extension configuration.
    pub config: ExecutionUpgradeSignalConfig,
}

impl ExecutionUpgradeSignalExtension {
    /// Creates a new execution upgrade signal extension.
    pub const fn new(config: ExecutionUpgradeSignalConfig) -> Self {
        Self { config }
    }

    /// Applies the configured L1 upgrade signal to the chain spec before startup.
    pub async fn apply_initial_signal_to_chain_spec(
        config: &ExecutionUpgradeSignalConfig,
        chain_spec: &mut BaseChainSpec,
    ) -> Result<(), UpgradeSignalError> {
        let reader = AlloyUpgradeSignalReader::new(
            RootProvider::new_http(config.l1_rpc.clone()),
            config.signal_config.contract_address,
        );
        let schedule = reader.read_schedule(&config.signal_config.hardfork_ids).await?;

        Self::apply_schedule_to_chain_spec(chain_spec, &schedule);

        Ok(())
    }

    /// Applies a contract-backed hardfork activation schedule to an execution chain spec.
    pub fn apply_schedule_to_chain_spec(
        chain_spec: &mut BaseChainSpec,
        schedule: &UpgradeSignalSchedule,
    ) -> usize {
        let mut applied = 0;
        let mut cleared = 0;
        for signal in &schedule.signals {
            let Some(timestamp) = signal.positive_activation_timestamp() else {
                if !chain_spec.clear_hardfork_activation_timestamp(&signal.hardfork_id) {
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
            if !chain_spec.set_hardfork_activation_timestamp(&signal.hardfork_id, timestamp) {
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

        applied
    }

    /// Polls L1 upgrade signal state.
    pub async fn poll_l1_signal(
        monitor: &mut UpgradeSignalMonitor,
        reader: &AlloyUpgradeSignalReader,
    ) {
        match reader.read_schedule(&monitor.config.hardfork_ids).await {
            Ok(schedule) => {
                monitor.update_schedule(schedule);
            }
            Err(error) => {
                monitor.record_l1_read_error(&error);
            }
        }
    }
}

impl BaseNodeExtension for ExecutionUpgradeSignalExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let config = self.config;

        hooks.add_node_started_hook(move |ctx| {
            let reader = AlloyUpgradeSignalReader::new(
                RootProvider::new_http(config.l1_rpc.clone()),
                config.signal_config.contract_address,
            );
            let mut monitor = UpgradeSignalMonitor::new(config.signal_config);
            let mut canonical_stream =
                BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
            let executor = ctx.task_executor;

            executor.spawn_with_graceful_shutdown_signal(|signal| {
                Box::pin(async move {
                    let mut interval = tokio::time::interval(DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL);
                    let mut signal = Box::pin(signal);

                    loop {
                        tokio::select! {
                            _ = &mut signal => break,
                            _ = interval.tick() => {
                                Self::poll_l1_signal(&mut monitor, &reader).await;
                            }
                            update = canonical_stream.next() => {
                                let Some(update) = update else {
                                    warn!(
                                        target: "upgrade_signal",
                                        "canonical state stream closed"
                                    );
                                    break;
                                };
                                let Ok(notification) = update else {
                                    continue;
                                };
                                let committed = notification.committed();
                                for block in committed.blocks_iter() {
                                    monitor.observe_l2_timestamp(block.timestamp);
                                }
                            }
                        }
                    }
                })
            });

            info!(target: "upgrade_signal", "execution upgrade signal observer spawned");
            Ok(())
        })
    }
}

impl FromExtensionConfig for ExecutionUpgradeSignalExtension {
    type Config = ExecutionUpgradeSignalConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

#[cfg(test)]
mod tests {
    use base_common_chains::BaseUpgrade;
    use reth_chainspec::{EthereumHardfork, ForkCondition, Hardforks};

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

        let applied = ExecutionUpgradeSignalExtension::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("canyon", 40), ("azul", 42)]),
        );

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

        let applied = ExecutionUpgradeSignalExtension::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("azul", 0)]),
        );

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

        let applied = ExecutionUpgradeSignalExtension::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[("delta", 42)]),
        );

        assert_eq!(applied, 0);
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }
}
