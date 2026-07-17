//! Upgrade signal observer actor: startup read, live metrics, and runtime auto-apply.

use alloy_provider::RootProvider;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalError,
    UpgradeSignalMetricLayer, UpgradeSignalMonitor, UpgradeSignalRefresher,
    UpgradeSignalRuntimeValidation,
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
    /// L1 provider used for upgrade signal reads.
    pub l1_provider: RootProvider,
    /// L2 chain ID.
    pub chain_id: u64,
    /// Runtime validation context.
    pub runtime_validation: UpgradeSignalRuntimeValidation,
}

impl UpgradeSignalNodeConfig {
    /// Builds consensus upgrade signal config from builder inputs.
    ///
    /// Uses `l1_rpc` when provided, otherwise falls back to the node's L1 provider. Missing runtime
    /// validation is fail-closed so positive Beryl signals are rejected without an activation admin.
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

    /// Builds the consensus metrics actor, with live auto-apply when runtime refresh is enabled.
    pub fn metrics_actor(
        &self,
        refresher: Option<UpgradeSignalRefresher>,
        cancellation: CancellationToken,
    ) -> UpgradeSignalMetricsActor {
        UpgradeSignalMetricsActor::new(
            self.config.clone(),
            self.l1_provider.clone(),
            refresher,
            cancellation,
        )
    }

    /// Builds the runtime admin refresher when enabled.
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

/// Actor that reads the L1 upgrade signal at startup, records live metrics, and, when runtime
/// refresh is enabled, automatically re-applies the schedule on observed L1 changes.
#[derive(Debug)]
pub struct UpgradeSignalMetricsActor {
    /// Upgrade signal schedule read configuration.
    pub config: UpgradeSignalConfig,
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// Live metrics state.
    pub monitor: UpgradeSignalMonitor,
    /// Runtime refresher applied automatically on observed live updates, when enabled.
    pub refresher: Option<UpgradeSignalRefresher>,
    /// Cancellation token shared with the rollup node.
    pub cancellation: CancellationToken,
}

impl UpgradeSignalMetricsActor {
    /// Creates a new upgrade signal metrics actor.
    pub fn new(
        config: UpgradeSignalConfig,
        l1_provider: RootProvider,
        refresher: Option<UpgradeSignalRefresher>,
        cancellation: CancellationToken,
    ) -> Self {
        let reader = config.reader(l1_provider);
        let monitor =
            UpgradeSignalMonitor::new(UpgradeSignalMetricLayer::Consensus, &config.upgrade_ids);

        Self { config, reader, monitor, refresher, cancellation }
    }

    /// Reads the L1 upgrade signal once before the node starts serving.
    ///
    /// A node — especially a sequencer that may immediately become leader — must never start with
    /// an unread upgrade schedule: a restart shortly before an upgrade could otherwise build
    /// blocks without the activation timestamp set and fork the chain. The read is retried and,
    /// on persistent failure, fails node startup instead of degrading to tolerant live polling.
    pub async fn read_startup_signal(&self) -> Result<(), UpgradeSignalError> {
        self.config
            .read_schedule(
                &self.reader,
                "consensus startup",
                &[UpgradeSignalMetricLayer::Consensus],
            )
            .await?;

        Ok(())
    }

    /// Polls L1 upgrade signal state, records metrics, and auto-applies observed changes when
    /// runtime refresh is enabled.
    pub async fn poll_l1_signal(&mut self) {
        let Some(schedule) = self.monitor.poll(&self.reader, &self.config.upgrade_ids).await else {
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

        // Ensure the upgrade signal is read on startup: a failed read must stop the node rather
        // than let it (and especially a sequencer leader) run with an unknown upgrade schedule.
        tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            result = self.read_startup_signal() => result?,
        }

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
