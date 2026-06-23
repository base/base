//! Derived L2 block parity monitoring.

use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};

use alloy_primitives::B256;
use alloy_provider::Provider;
use alloy_rpc_types_eth::{Block, BlockNumberOrTag};
use async_trait::async_trait;
use base_common_network::Base;
use base_common_rpc_types::Transaction;
use futures::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::L2BlockParityMetrics;

/// Default maximum derived L2 blocks compared in one monitor pass.
pub const DEFAULT_MAX_BLOCKS_PER_TICK: usize = 25;

/// Provider abstraction for derived L2 block parity checks.
#[async_trait]
pub trait L2BlockProvider: Send + Sync {
    /// Fetch the latest L2 block number from this provider.
    async fn latest_block_number(&self) -> eyre::Result<u64>;

    /// Fetch an L2 block snapshot by number.
    async fn block_by_number(&self, number: u64) -> eyre::Result<Option<L2BlockSnapshot>>;
}

/// RPC-backed derived L2 block provider.
#[derive(Clone, derive_more::Debug)]
pub struct RpcL2BlockProvider {
    /// L2 RPC provider.
    #[debug(skip)]
    pub provider: Arc<dyn Provider<Base> + Send + Sync>,
}

impl RpcL2BlockProvider {
    /// Creates a new RPC-backed L2 block provider.
    pub const fn new(provider: Arc<dyn Provider<Base> + Send + Sync>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl L2BlockProvider for RpcL2BlockProvider {
    async fn latest_block_number(&self) -> eyre::Result<u64> {
        self.provider
            .get_block_number()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L2 latest block number: {e}"))
    }

    async fn block_by_number(&self, number: u64) -> eyre::Result<Option<L2BlockSnapshot>> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .full()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L2 block {number}: {e}"))?;
        Ok(block.map(L2BlockSnapshot::from_rpc_block))
    }
}

/// Lightweight derived L2 block data used for parity comparison and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L2BlockSnapshot {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Block timestamp.
    pub timestamp: u64,
    /// Ordered transaction hashes.
    pub tx_hashes: Vec<B256>,
}

impl L2BlockSnapshot {
    /// Converts an RPC block into a parity snapshot.
    pub fn from_rpc_block(block: Block<Transaction>) -> Self {
        Self {
            number: block.header.number,
            hash: block.header.hash,
            parent_hash: block.header.inner.parent_hash,
            timestamp: block.header.inner.timestamp,
            tx_hashes: block.transactions.hashes().collect(),
        }
    }

    /// Returns the transaction count.
    pub const fn tx_count(&self) -> usize {
        self.tx_hashes.len()
    }

    /// Returns the first transaction-hash mismatch, if any.
    pub fn first_tx_hash_mismatch(
        &self,
        other: &Self,
    ) -> Option<(usize, Option<B256>, Option<B256>)> {
        let max_len = self.tx_hashes.len().max(other.tx_hashes.len());
        (0..max_len).find_map(|index| {
            let left = self.tx_hashes.get(index).copied();
            let right = other.tx_hashes.get(index).copied();
            (left != right).then_some((index, left, right))
        })
    }
}

/// Runtime configuration for derived L2 block parity monitoring.
#[derive(Debug, Clone)]
pub struct L2BlockParityMonitorConfig {
    /// L2 block number where monitoring starts.
    pub start_block: u64,
    /// Monitor polling interval.
    pub poll_interval: Duration,
    /// Maximum number of blocks compared in one monitor pass.
    pub max_blocks_per_tick: usize,
}

impl L2BlockParityMonitorConfig {
    /// Creates a new monitor config with the default per-tick block cap.
    pub const fn new(start_block: u64, poll_interval: Duration) -> Self {
        Self { start_block, poll_interval, max_blocks_per_tick: DEFAULT_MAX_BLOCKS_PER_TICK }
    }

    /// Returns the validated per-tick block cap.
    pub fn validated_max_blocks_per_tick(&self) -> eyre::Result<u64> {
        if self.max_blocks_per_tick == 0 {
            eyre::bail!("max_blocks_per_tick must be greater than 0");
        }
        Ok(self.max_blocks_per_tick as u64)
    }
}

/// Result counts from one derived L2 block parity pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct L2BlockParityStats {
    /// Number of blocks compared.
    pub checked: usize,
    /// Number of matching block hashes.
    pub matches: usize,
    /// Number of mismatching block hashes.
    pub mismatches: usize,
    /// Number of blocks unavailable from one or both sides.
    pub missing_blocks: usize,
}

impl L2BlockParityStats {
    /// Returns true if this pass found no mismatch or missing block.
    pub const fn is_aligned(self) -> bool {
        self.mismatches == 0 && self.missing_blocks == 0
    }
}

/// One derived L2 block parity comparison result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L2BlockParityResult {
    /// Both sides returned the block and hashes matched.
    Match {
        /// Compared block number.
        number: u64,
        /// Matching block hash.
        hash: B256,
    },
    /// Both sides returned the block but hashes diverged.
    Mismatch {
        /// Sequencer block snapshot.
        sequencer: L2BlockSnapshot,
        /// Shadow validator block snapshot.
        validator: L2BlockSnapshot,
    },
    /// One or both sides did not return the block.
    Missing {
        /// Compared block number.
        number: u64,
        /// Whether the sequencer did not return the block.
        sequencer_missing: bool,
        /// Whether the validator did not return the block.
        validator_missing: bool,
    },
}

impl L2BlockParityResult {
    /// Compared L2 block number.
    pub const fn number(&self) -> u64 {
        match self {
            Self::Match { number, .. } | Self::Missing { number, .. } => *number,
            Self::Mismatch { sequencer, .. } => sequencer.number,
        }
    }
}

/// Derived L2 block parity monitor.
#[derive(Debug)]
pub struct L2BlockParityMonitor<S, V> {
    /// Sequencer L2 block provider.
    pub sequencer: S,
    /// Shadow parity-validator L2 block provider.
    pub validator: V,
    /// Monitor configuration.
    pub config: L2BlockParityMonitorConfig,
    /// Next L2 block number to compare.
    pub next_block: u64,
}

impl<S, V> L2BlockParityMonitor<S, V>
where
    S: L2BlockProvider + 'static,
    V: L2BlockProvider + 'static,
{
    /// Creates a new derived L2 block parity monitor.
    pub const fn new(sequencer: S, validator: V, config: L2BlockParityMonitorConfig) -> Self {
        Self { next_block: config.start_block, sequencer, validator, config }
    }

    /// Spawns the monitor onto the Tokio runtime.
    pub fn spawn(mut self, cancellation: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(panic) = AssertUnwindSafe(self.run(cancellation)).catch_unwind().await {
                L2BlockParityMetrics::enabled().set(0.0);
                error!("derived L2 block parity monitor panicked");
                std::panic::resume_unwind(panic);
            }
        })
    }

    /// Runs the monitor loop until cancelled.
    pub async fn run(&mut self, cancellation: CancellationToken) {
        L2BlockParityMetrics::enabled().set(1.0);
        info!(
            start_block = %self.next_block,
            poll_interval_ms = %self.config.poll_interval.as_millis(),
            max_blocks_per_tick = %self.config.max_blocks_per_tick,
            "derived L2 block parity monitor started"
        );

        loop {
            if cancellation.is_cancelled() {
                break;
            }

            if let Err(error) = self.process_once().await {
                L2BlockParityMetrics::fetch_errors_total().increment(1);
                error!(error = %error, "derived L2 block parity pass failed");
            }

            if !Self::sleep_or_cancel(self.config.poll_interval, &cancellation).await {
                break;
            }
        }

        L2BlockParityMetrics::enabled().set(0.0);
        info!("derived L2 block parity monitor stopped");
    }

    /// Processes one parity pass.
    pub async fn process_once(&mut self) -> eyre::Result<L2BlockParityStats> {
        let sequencer_latest = self.sequencer.latest_block_number().await?;
        let validator_latest = self.validator.latest_block_number().await?;
        // Metrics gauges are f64-backed; integer block numbers remain exact
        // through 2^53, far above any realistic L2 height for this monitor.
        L2BlockParityMetrics::sequencer_latest_l2_block().set(sequencer_latest as f64);
        L2BlockParityMetrics::validator_latest_l2_block().set(validator_latest as f64);
        L2BlockParityMetrics::lag_blocks()
            .set(sequencer_latest.saturating_sub(validator_latest) as f64);

        let common_latest = sequencer_latest.min(validator_latest);
        if common_latest < self.next_block {
            let aligned = if sequencer_latest == validator_latest { 1.0 } else { 0.0 };
            L2BlockParityMetrics::aligned().set(aligned);
            debug!(
                next_block = %self.next_block,
                sequencer_latest = %sequencer_latest,
                validator_latest = %validator_latest,
                "waiting for parity validator to reach next comparable L2 block"
            );
            return Ok(L2BlockParityStats::default());
        }

        let max_blocks = self.config.validated_max_blocks_per_tick()?;
        let last_block = common_latest.min(self.next_block.saturating_add(max_blocks - 1));
        let mut stats = L2BlockParityStats::default();

        for number in self.next_block..=last_block {
            match self.compare_block(number).await {
                Ok(result) => {
                    let next_block = result.number().saturating_add(1);
                    self.record_result(result, &mut stats);
                    self.next_block = next_block;
                }
                Err(error) => {
                    L2BlockParityMetrics::fetch_errors_total().increment(1);
                    warn!(
                        error = %error,
                        l2_block = %number,
                        "derived L2 block parity fetch failed; will retry"
                    );
                    break;
                }
            }
        }

        let caught_up = self.next_block > common_latest;
        let aligned = if caught_up && stats.is_aligned() && sequencer_latest == validator_latest {
            1.0
        } else {
            0.0
        };
        L2BlockParityMetrics::aligned().set(aligned);

        if stats.checked > 0 || stats.missing_blocks > 0 {
            debug!(
                checked = %stats.checked,
                matches = %stats.matches,
                mismatches = %stats.mismatches,
                missing_blocks = %stats.missing_blocks,
                next_block = %self.next_block,
                sequencer_latest = %sequencer_latest,
                validator_latest = %validator_latest,
                "derived L2 block parity pass processed"
            );
        }

        Ok(stats)
    }

    /// Compares one L2 block number.
    pub async fn compare_block(&self, number: u64) -> eyre::Result<L2BlockParityResult> {
        let (sequencer, validator) = tokio::join!(
            self.sequencer.block_by_number(number),
            self.validator.block_by_number(number),
        );
        let sequencer = sequencer?;
        let validator = validator?;

        match (sequencer, validator) {
            (Some(sequencer), Some(validator)) if sequencer.hash == validator.hash => {
                Ok(L2BlockParityResult::Match { number, hash: sequencer.hash })
            }
            (Some(sequencer), Some(validator)) => {
                Ok(L2BlockParityResult::Mismatch { sequencer, validator })
            }
            (sequencer, validator) => Ok(L2BlockParityResult::Missing {
                number,
                sequencer_missing: sequencer.is_none(),
                validator_missing: validator.is_none(),
            }),
        }
    }

    /// Records one comparison result into metrics and stats.
    pub fn record_result(&self, result: L2BlockParityResult, stats: &mut L2BlockParityStats) {
        match result {
            L2BlockParityResult::Match { number, hash } => {
                stats.checked += 1;
                stats.matches += 1;
                L2BlockParityMetrics::checked_total().increment(1);
                L2BlockParityMetrics::matches_total().increment(1);
                L2BlockParityMetrics::last_checked_l2_block().set(number as f64);
                L2BlockParityMetrics::last_match_l2_block().set(number as f64);
                debug!(
                    l2_block = %number,
                    hash = %hash,
                    "derived L2 block parity matched"
                );
            }
            L2BlockParityResult::Mismatch { sequencer, validator } => {
                stats.checked += 1;
                stats.mismatches += 1;
                L2BlockParityMetrics::checked_total().increment(1);
                L2BlockParityMetrics::mismatches_total().increment(1);
                L2BlockParityMetrics::last_checked_l2_block().set(sequencer.number as f64);
                L2BlockParityMetrics::last_mismatch_l2_block().set(sequencer.number as f64);
                let tx_mismatch = sequencer.first_tx_hash_mismatch(&validator);
                warn!(
                    l2_block = %sequencer.number,
                    sequencer_hash = %sequencer.hash,
                    validator_hash = %validator.hash,
                    sequencer_parent_hash = %sequencer.parent_hash,
                    validator_parent_hash = %validator.parent_hash,
                    sequencer_timestamp = %sequencer.timestamp,
                    validator_timestamp = %validator.timestamp,
                    sequencer_tx_count = %sequencer.tx_count(),
                    validator_tx_count = %validator.tx_count(),
                    first_tx_mismatch = ?tx_mismatch,
                    "derived L2 block parity mismatch"
                );
            }
            L2BlockParityResult::Missing { number, sequencer_missing, validator_missing } => {
                stats.missing_blocks += 1;
                L2BlockParityMetrics::missing_blocks_total().increment(1);
                warn!(
                    l2_block = %number,
                    sequencer_missing = %sequencer_missing,
                    validator_missing = %validator_missing,
                    "derived L2 block parity skipped missing block"
                );
            }
        }
    }

    /// Sleeps for the interval or returns false if cancellation fires.
    pub async fn sleep_or_cancel(interval: Duration, cancellation: &CancellationToken) -> bool {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => false,
            () = tokio::time::sleep(interval) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Arc,
    };

    use tokio::sync::Mutex;

    use super::*;

    #[derive(Debug, Default)]
    struct MockL2BlockProvider {
        latest: u64,
        blocks: BTreeMap<u64, L2BlockSnapshot>,
        fail_blocks: BTreeSet<u64>,
    }

    impl MockL2BlockProvider {
        fn new(latest: u64, blocks: impl IntoIterator<Item = L2BlockSnapshot>) -> Self {
            Self {
                latest,
                blocks: blocks.into_iter().map(|block| (block.number, block)).collect(),
                fail_blocks: BTreeSet::new(),
            }
        }

        fn with_fail_blocks(mut self, fail_blocks: impl IntoIterator<Item = u64>) -> Self {
            self.fail_blocks = fail_blocks.into_iter().collect();
            self
        }
    }

    #[async_trait]
    impl L2BlockProvider for Arc<Mutex<MockL2BlockProvider>> {
        async fn latest_block_number(&self) -> eyre::Result<u64> {
            Ok(self.lock().await.latest)
        }

        async fn block_by_number(&self, number: u64) -> eyre::Result<Option<L2BlockSnapshot>> {
            let provider = self.lock().await;
            if provider.fail_blocks.contains(&number) {
                eyre::bail!("mock fetch failed for block {number}");
            }
            Ok(provider.blocks.get(&number).cloned())
        }
    }

    fn snapshot(number: u64, hash_byte: u8, tx_hashes: &[u8]) -> L2BlockSnapshot {
        L2BlockSnapshot {
            number,
            hash: B256::repeat_byte(hash_byte),
            parent_hash: B256::repeat_byte(hash_byte.saturating_sub(1)),
            timestamp: number * 2,
            tx_hashes: tx_hashes.iter().copied().map(B256::repeat_byte).collect(),
        }
    }

    #[tokio::test]
    async fn process_once_records_matching_blocks() {
        let sequencer = Arc::new(Mutex::new(MockL2BlockProvider::new(
            2,
            [snapshot(1, 1, &[10]), snapshot(2, 2, &[20])],
        )));
        let validator = Arc::new(Mutex::new(MockL2BlockProvider::new(
            2,
            [snapshot(1, 1, &[10]), snapshot(2, 2, &[20])],
        )));
        let config = L2BlockParityMonitorConfig {
            start_block: 1,
            max_blocks_per_tick: 10,
            ..L2BlockParityMonitorConfig::new(1, Duration::from_secs(1))
        };
        let mut monitor = L2BlockParityMonitor::new(sequencer, validator, config);

        let stats = monitor.process_once().await.unwrap();

        assert_eq!(stats.checked, 2);
        assert_eq!(stats.matches, 2);
        assert_eq!(stats.mismatches, 0);
        assert_eq!(monitor.next_block, 3);
    }

    #[tokio::test]
    async fn process_once_keeps_progress_before_fetch_error() {
        let sequencer = Arc::new(Mutex::new(MockL2BlockProvider::new(
            3,
            [snapshot(1, 1, &[10]), snapshot(2, 2, &[20]), snapshot(3, 3, &[30])],
        )));
        let validator = Arc::new(Mutex::new(
            MockL2BlockProvider::new(
                3,
                [snapshot(1, 1, &[10]), snapshot(2, 2, &[20]), snapshot(3, 3, &[30])],
            )
            .with_fail_blocks([3]),
        ));
        let config = L2BlockParityMonitorConfig {
            start_block: 1,
            max_blocks_per_tick: 10,
            ..L2BlockParityMonitorConfig::new(1, Duration::from_secs(1))
        };
        let mut monitor = L2BlockParityMonitor::new(sequencer, validator, config);

        let stats = monitor.process_once().await.unwrap();

        assert_eq!(stats.checked, 2);
        assert_eq!(stats.matches, 2);
        assert_eq!(monitor.next_block, 3);
    }

    #[tokio::test]
    async fn process_once_records_mismatching_blocks() {
        let sequencer = Arc::new(Mutex::new(MockL2BlockProvider::new(1, [snapshot(1, 1, &[10])])));
        let validator = Arc::new(Mutex::new(MockL2BlockProvider::new(1, [snapshot(1, 2, &[11])])));
        let config = L2BlockParityMonitorConfig::new(1, Duration::from_secs(1));
        let mut monitor = L2BlockParityMonitor::new(sequencer, validator, config);

        let stats = monitor.process_once().await.unwrap();

        assert_eq!(stats.checked, 1);
        assert_eq!(stats.matches, 0);
        assert_eq!(stats.mismatches, 1);
        assert_eq!(monitor.next_block, 2);
    }

    #[tokio::test]
    async fn process_once_waits_for_validator_to_reach_next_block() {
        let sequencer = Arc::new(Mutex::new(MockL2BlockProvider::new(5, [snapshot(5, 5, &[50])])));
        let validator = Arc::new(Mutex::new(MockL2BlockProvider::new(3, [])));
        let config = L2BlockParityMonitorConfig::new(4, Duration::from_secs(1));
        let mut monitor = L2BlockParityMonitor::new(sequencer, validator, config);

        let stats = monitor.process_once().await.unwrap();

        assert_eq!(stats, L2BlockParityStats::default());
        assert_eq!(monitor.next_block, 4);
    }

    #[tokio::test]
    async fn process_once_rejects_zero_max_blocks_per_tick() {
        let sequencer = Arc::new(Mutex::new(MockL2BlockProvider::new(1, [snapshot(1, 1, &[10])])));
        let validator = Arc::new(Mutex::new(MockL2BlockProvider::new(1, [snapshot(1, 1, &[10])])));
        let config = L2BlockParityMonitorConfig {
            max_blocks_per_tick: 0,
            ..L2BlockParityMonitorConfig::new(1, Duration::from_secs(1))
        };
        let mut monitor = L2BlockParityMonitor::new(sequencer, validator, config);

        let err = monitor
            .process_once()
            .await
            .expect_err("zero max_blocks_per_tick should fail validation");

        assert!(err.to_string().contains("max_blocks_per_tick must be greater than 0"));
    }
}
