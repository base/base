//! Upgrade signal metrics observer actor.

use alloy_provider::RootProvider;
use base_common_genesis::BaseUpgrade;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalError,
    UpgradeSignalMonitor,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::NodeActor;

/// Actor that records live L1 upgrade signal metrics without mutating node configuration.
#[derive(Debug)]
pub struct UpgradeSignalMetricsActor {
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// Contract-backed upgrades read from the L1 contract.
    pub hardfork_ids: Vec<BaseUpgrade>,
    /// Live metrics state.
    pub monitor: UpgradeSignalMonitor,
    /// Cancellation token shared with the rollup node.
    pub cancellation: CancellationToken,
}

impl UpgradeSignalMetricsActor {
    /// Creates a new upgrade signal metrics actor.
    pub fn new(
        config: UpgradeSignalConfig,
        l1_provider: RootProvider,
        cancellation: CancellationToken,
    ) -> Self {
        let reader = AlloyUpgradeSignalReader::new(l1_provider, config.contract_address)
            .with_block_tag(config.l1_block_tag);
        let monitor = UpgradeSignalMonitor::new(&config.hardfork_ids);

        Self { reader, hardfork_ids: config.hardfork_ids, monitor, cancellation }
    }

    /// Polls L1 upgrade signal state and records metrics without mutating local config.
    pub async fn poll_l1_signal(&mut self) {
        let updated_signals = self.monitor.poll(&self.reader, &self.hardfork_ids).await;
        if updated_signals > 0 {
            info!(
                target: "upgrade_signal",
                updated_signals,
                "observed live L1 upgrade signal update"
            );
        }
    }
}

#[async_trait::async_trait]
impl NodeActor for UpgradeSignalMetricsActor {
    type StartData = ();
    type Error = UpgradeSignalError;

    async fn start(mut self, _ctx: ()) -> Result<(), Self::Error> {
        let mut interval = tokio::time::interval(UpgradeSignalDefaults::POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    self.poll_l1_signal().await;
                }
            }
        }
    }
}
