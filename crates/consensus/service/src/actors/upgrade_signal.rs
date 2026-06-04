//! Upgrade signal metrics observer actor.

use alloy_provider::RootProvider;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL, UpgradeSignalConfig,
    UpgradeSignalError, UpgradeSignalMetrics, UpgradeSignalMonitor, UpgradeSignalStateUpdate,
};
use tokio_util::sync::CancellationToken;

use crate::NodeActor;

/// Actor that records live L1 upgrade signal metrics without mutating node configuration.
#[derive(Debug)]
pub struct UpgradeSignalMetricsActor {
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// Hardfork IDs read from the L1 contract.
    pub hardfork_ids: Vec<String>,
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
        let reader = AlloyUpgradeSignalReader::new(l1_provider, config.contract_address);
        let monitor = UpgradeSignalMonitor::new(&config.hardfork_ids);

        Self { reader, hardfork_ids: config.hardfork_ids, monitor, cancellation }
    }

    /// Polls L1 upgrade signal state and records metrics without mutating local config.
    pub async fn poll_l1_signal(&mut self) {
        match self.reader.read_schedule(&self.hardfork_ids).await {
            Ok(schedule) => {
                let updates = self.monitor.update_schedule(schedule);
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
                UpgradeSignalMetrics::record_l1_read_errors(&self.hardfork_ids);
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    hardfork_ids = ?self.hardfork_ids,
                    "failed to read live L1 upgrade signal metrics"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl NodeActor for UpgradeSignalMetricsActor {
    type StartData = ();
    type Error = UpgradeSignalError;

    async fn start(mut self, _ctx: ()) -> Result<(), Self::Error> {
        let mut interval = tokio::time::interval(DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL);

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
