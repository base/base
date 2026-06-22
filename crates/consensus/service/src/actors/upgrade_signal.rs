//! Upgrade signal metrics observer actor.

use alloy_provider::RootProvider;
use base_common_genesis::BaseUpgrade;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalError,
    UpgradeSignalMetricLayer, UpgradeSignalMonitor,
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
    pub upgrade_ids: Vec<BaseUpgrade>,
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
        let reader = config.reader(l1_provider);
        let monitor =
            UpgradeSignalMonitor::new(UpgradeSignalMetricLayer::Consensus, &config.upgrade_ids);

        Self { reader, upgrade_ids: config.upgrade_ids, monitor, cancellation }
    }

    /// Polls L1 upgrade signal state and records metrics without mutating local config.
    pub async fn poll_l1_signal(&mut self) {
        let updated_signals = self.monitor.poll(&self.reader, &self.upgrade_ids).await;
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
        let cancellation = self.cancellation.clone();
        let mut interval = tokio::time::interval(UpgradeSignalDefaults::POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    tokio::select! {
                        _ = cancellation.cancelled() => return Ok(()),
                        _ = self.poll_l1_signal() => {}
                    }
                }
            }
        }
    }
}
