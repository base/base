//! Upgrade signal metrics observer actor.

use core::time::Duration;

use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalError, UpgradeSignalMetricLayer,
    UpgradeSignalMonitor, UpgradeSignalRefresher,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::Url;

use crate::NodeActor;

/// Upgrade signal config resolved for a running consensus node.
#[derive(Debug, Clone)]
pub struct UpgradeSignalNodeConfig {
    /// Schedule read configuration.
    pub config: UpgradeSignalConfig,
    /// Shared L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// L2 chain ID.
    pub chain_id: u64,
}

impl UpgradeSignalNodeConfig {
    /// Builds consensus upgrade signal config from builder inputs.
    ///
    /// Uses `l1_rpc` when provided, otherwise falls back to the node's L1 RPC URL.
    pub fn resolve(
        config: UpgradeSignalConfig,
        l1_rpc: Option<&Url>,
        default_l1_rpc: Url,
        chain_id: u64,
    ) -> Result<Self, UpgradeSignalError> {
        let l1_rpc = l1_rpc.cloned().unwrap_or(default_l1_rpc);
        let reader = config.reader(l1_rpc)?;
        Ok(Self { config, reader, chain_id })
    }

    /// Builds the consensus metrics actor, with live auto-apply when runtime refresh is enabled.
    pub fn metrics_actor(
        &self,
        refresher: Option<UpgradeSignalRefresher>,
        cancellation: CancellationToken,
    ) -> UpgradeSignalMetricsActor {
        UpgradeSignalMetricsActor::new(
            self.reader.clone(),
            self.config.l1_block_tag.poll_interval(),
            refresher,
            cancellation,
        )
    }

    /// Builds the runtime admin refresher when enabled.
    pub fn refresher(&self) -> Option<UpgradeSignalRefresher> {
        self.config.mode.allows_runtime_admin().then(|| {
            UpgradeSignalRefresher::new(
                self.config.clone(),
                self.reader.clone(),
                self.chain_id,
                UpgradeSignalMetricLayer::Consensus,
            )
        })
    }
}

/// Actor that records live L1 upgrade signal metrics and, when runtime refresh is enabled,
/// automatically re-applies the schedule on observed L1 changes.
#[derive(Debug)]
pub struct UpgradeSignalMetricsActor {
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// Live metrics state.
    pub monitor: UpgradeSignalMonitor,
    /// Interval between live L1 upgrade signal contract reads.
    pub poll_interval: Duration,
    /// Runtime refresher applied automatically on observed live updates, when enabled.
    pub refresher: Option<UpgradeSignalRefresher>,
    /// Cancellation token shared with the rollup node.
    pub cancellation: CancellationToken,
}

impl UpgradeSignalMetricsActor {
    /// Creates a new upgrade signal metrics actor.
    pub fn new(
        reader: AlloyUpgradeSignalReader,
        poll_interval: Duration,
        refresher: Option<UpgradeSignalRefresher>,
        cancellation: CancellationToken,
    ) -> Self {
        let monitor = UpgradeSignalMonitor::new(UpgradeSignalMetricLayer::Consensus);

        Self { reader, monitor, poll_interval, refresher, cancellation }
    }

    /// Polls L1 upgrade signal state, records metrics, and auto-applies observed changes when
    /// runtime refresh is enabled.
    pub async fn poll_l1_signal(&mut self) {
        let Some(schedule) = self.monitor.poll(&self.reader).await else {
            return;
        };
        if let Some(refresher) = &self.refresher
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

#[async_trait::async_trait]
impl NodeActor for UpgradeSignalMetricsActor {
    type StartData = ();
    type Error = UpgradeSignalError;

    async fn start(mut self, _ctx: ()) -> Result<(), Self::Error> {
        let cancellation = self.cancellation.clone();
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }

            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = self.poll_l1_signal() => {}
            }
        }
    }
}
