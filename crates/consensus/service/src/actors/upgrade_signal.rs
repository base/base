//! Upgrade signal metrics observer actor.

use alloy_provider::RootProvider;
use base_common_genesis::BaseUpgrade;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalError,
    UpgradeSignalMetricLayer, UpgradeSignalMonitor, UpgradeSignalRefresher,
    UpgradeSignalRuntimeValidation,
};
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

use crate::NodeActor;

/// Consolidated upgrade signal configuration for the consensus node.
///
/// Bundles the core schedule config with the resolved L1 provider, chain ID, and runtime
/// validation context, so that actor and refresher construction is a single method call.
#[derive(Debug, Clone)]
pub struct UpgradeSignalNodeConfig {
    /// Core upgrade signal schedule read configuration.
    pub config: UpgradeSignalConfig,
    /// Resolved L1 provider used for upgrade signal reads.
    pub l1_provider: RootProvider,
    /// L2 chain ID.
    pub chain_id: u64,
    /// Runtime upgrade signal validation context.
    pub runtime_validation: UpgradeSignalRuntimeValidation,
}

impl UpgradeSignalNodeConfig {
    /// Resolves a node upgrade signal config from optional builder inputs.
    ///
    /// Reads use the dedicated `l1_rpc` endpoint when set, otherwise falling back to
    /// `default_l1_provider` (the node's L1 engine provider). When no `runtime_validation` is
    /// supplied, a fail-closed context is used.
    pub fn resolve(
        config: UpgradeSignalConfig,
        l1_rpc: Option<&Url>,
        default_l1_provider: RootProvider,
        chain_id: u64,
        runtime_validation: Option<UpgradeSignalRuntimeValidation>,
    ) -> Self {
        let l1_provider =
            l1_rpc.map(|url| RootProvider::new_http(url.clone())).unwrap_or(default_l1_provider);
        let runtime_validation =
            runtime_validation.unwrap_or_else(UpgradeSignalRuntimeValidation::fail_closed);
        Self { config, l1_provider, chain_id, runtime_validation }
    }

    /// Creates the upgrade signal metrics actor.
    pub fn metrics_actor(&self, cancellation: CancellationToken) -> UpgradeSignalMetricsActor {
        UpgradeSignalMetricsActor::new(self.config.clone(), self.l1_provider.clone(), cancellation)
    }

    /// Creates the upgrade signal refresher if runtime admin mode is enabled.
    pub fn refresher(&self) -> Option<UpgradeSignalRefresher> {
        self.config.mode.allows_runtime_admin().then(|| {
            UpgradeSignalRefresher::new(
                self.config.clone(),
                self.l1_provider.clone(),
                self.chain_id,
                self.runtime_validation,
                UpgradeSignalMetricLayer::Consensus,
            )
        })
    }
}

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
                _ = interval.tick() => {}
            }

            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = self.poll_l1_signal() => {}
            }
        }
    }
}
