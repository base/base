//! Upgrade signal observer actor.

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_network::Base;
use base_upgrade_signal::{
    AlloyUpgradeSignalReader, L2TimestampSource, UpgradeSignalConfig, UpgradeSignalError,
    UpgradeSignalMonitor, UpgradeSignalPollingObserver,
};
use tokio_util::sync::CancellationToken;

use crate::NodeActor;

/// L2 timestamp source backed by the consensus node's L2 provider.
#[derive(Debug, Clone)]
pub struct ConsensusL2TimestampSource {
    /// L2 execution provider.
    pub provider: RootProvider<Base>,
}

impl ConsensusL2TimestampSource {
    /// Creates a new L2 timestamp source.
    pub const fn new(provider: RootProvider<Base>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl L2TimestampSource for ConsensusL2TimestampSource {
    async fn latest_l2_timestamp(&self) -> Result<Option<u64>, UpgradeSignalError> {
        self.provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map(|block| block.map(|block| block.header.timestamp))
            .map_err(|error| UpgradeSignalError::provider("get latest L2 block failed", error))
    }
}

/// Actor that observes upgrade signals from L1 and local L2 timestamps.
#[derive(Debug)]
pub struct UpgradeSignalActor {
    /// Polling observer.
    pub observer:
        UpgradeSignalPollingObserver<AlloyUpgradeSignalReader, ConsensusL2TimestampSource>,
    /// Cancellation token shared with the rollup node.
    pub cancellation: CancellationToken,
}

impl UpgradeSignalActor {
    /// Creates a new upgrade signal actor.
    pub fn new(
        config: UpgradeSignalConfig,
        l1_provider: RootProvider,
        l2_provider: RootProvider<Base>,
        cancellation: CancellationToken,
    ) -> Self {
        let reader = AlloyUpgradeSignalReader::new(l1_provider, config.contract_address);
        let l2_timestamp_source = ConsensusL2TimestampSource::new(l2_provider);
        let monitor = UpgradeSignalMonitor::new(config);
        let observer = UpgradeSignalPollingObserver::new(reader, l2_timestamp_source, monitor);

        Self { observer, cancellation }
    }
}

#[async_trait]
impl NodeActor for UpgradeSignalActor {
    type StartData = ();
    type Error = UpgradeSignalError;

    async fn start(mut self, _ctx: ()) -> Result<(), Self::Error> {
        self.observer.run(self.cancellation).await
    }
}
