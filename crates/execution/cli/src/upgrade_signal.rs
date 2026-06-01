//! Execution-node upgrade signal extension.

use alloy_provider::RootProvider;
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL, UpgradeSignalConfig,
    UpgradeSignalMonitor, UpgradeSignalReader,
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
