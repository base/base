//! Polling upgrade signal observer.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{
    DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL, UpgradeSignalError, UpgradeSignalMonitor,
    UpgradeSignalReader,
};

/// Interface for reading the latest local L2 timestamp.
#[async_trait]
pub trait L2TimestampSource: Send + Sync {
    /// Returns the latest local L2 timestamp, or `None` if no local L2 block is available yet.
    async fn latest_l2_timestamp(&self) -> Result<Option<u64>, UpgradeSignalError>;
}

/// Periodically reads L1 upgrade signal state and observes local L2 timestamps.
#[derive(Debug)]
pub struct UpgradeSignalPollingObserver<R, S> {
    /// L1 upgrade signal reader.
    pub reader: R,
    /// L2 timestamp source.
    pub l2_timestamp_source: S,
    /// Signal monitor.
    pub monitor: UpgradeSignalMonitor,
}

impl<R, S> UpgradeSignalPollingObserver<R, S> {
    /// Creates a new polling observer.
    pub const fn new(reader: R, l2_timestamp_source: S, monitor: UpgradeSignalMonitor) -> Self {
        Self { reader, l2_timestamp_source, monitor }
    }
}

impl<R, S> UpgradeSignalPollingObserver<R, S>
where
    R: UpgradeSignalReader,
    S: L2TimestampSource,
{
    /// Runs the observer until `cancellation` is cancelled.
    pub async fn run(&mut self, cancellation: CancellationToken) -> Result<(), UpgradeSignalError> {
        let mut interval = tokio::time::interval(DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    self.tick().await;
                }
            }
        }
    }

    /// Performs one observer tick.
    pub async fn tick(&mut self) {
        self.poll_l1_signal().await;
        self.observe_l2_timestamp().await;
    }

    /// Polls the L1 signal.
    pub async fn poll_l1_signal(&mut self) {
        match self.reader.read_schedule(&self.monitor.config.hardfork_ids).await {
            Ok(schedule) => {
                self.monitor.update_schedule(schedule);
            }
            Err(error) => {
                self.monitor.record_l1_read_error(&error);
            }
        }
    }

    /// Observes the latest L2 timestamp.
    pub async fn observe_l2_timestamp(&mut self) {
        match self.l2_timestamp_source.latest_l2_timestamp().await {
            Ok(Some(timestamp)) => {
                self.monitor.observe_l2_timestamp(timestamp);
            }
            Ok(None) => {}
            Err(error) => {
                self.monitor.record_l2_timestamp_error(&error);
            }
        }
    }
}
