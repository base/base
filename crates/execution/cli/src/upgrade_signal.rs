//! Execution-node upgrade signal extension.

use alloy_provider::RootProvider;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL, UpgradeSignal,
    UpgradeSignalConfig, UpgradeSignalError, UpgradeSignalMonitor, UpgradeSignalReader,
};
use reth_chain_state::CanonStateSubscriptions;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::{info, warn};
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
        let signal = reader.read_signal(&config.signal_config.hardfork_id).await?;

        Self::apply_signal_to_chain_spec(chain_spec, &signal);

        Ok(())
    }

    /// Applies a positive Azul activation timestamp to an execution chain spec.
    pub fn apply_signal_to_chain_spec(
        chain_spec: &mut BaseChainSpec,
        signal: &UpgradeSignal,
    ) -> bool {
        let Some(timestamp) = signal.azul_activation_timestamp() else {
            return false;
        };

        chain_spec.set_azul_activation_timestamp(timestamp);
        info!(
            target: "upgrade_signal",
            hardfork_id = %signal.hardfork_id,
            activation_timestamp = %timestamp,
            "applied upgrade signal to execution chain spec"
        );

        true
    }

    /// Polls L1 upgrade signal state.
    pub async fn poll_l1_signal(
        monitor: &mut UpgradeSignalMonitor,
        reader: &AlloyUpgradeSignalReader,
    ) {
        match reader.read_signal(&monitor.config.hardfork_id).await {
            Ok(signal) => {
                monitor.update_signal(signal);
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

    fn signal(hardfork_id: &str, activation_timestamp: u64) -> UpgradeSignal {
        UpgradeSignal {
            hardfork_id: hardfork_id.to_string(),
            activation_timestamp,
            protocol_version: Default::default(),
            l1_block_number: 1,
        }
    }

    #[test]
    fn applies_positive_azul_signal_to_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignalExtension::apply_signal_to_chain_spec(
            &mut chain_spec,
            &signal("azul", 42),
        );

        assert!(applied);
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Timestamp(42));
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Timestamp(42));
    }

    #[test]
    fn ignores_zero_azul_signal_for_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignalExtension::apply_signal_to_chain_spec(
            &mut chain_spec,
            &signal("azul", 0),
        );

        assert!(!applied);
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }

    #[test]
    fn ignores_non_azul_signal_for_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignalExtension::apply_signal_to_chain_spec(
            &mut chain_spec,
            &signal("beryl", 42),
        );

        assert!(!applied);
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }
}
