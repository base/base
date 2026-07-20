use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use tracing::{debug, warn};

use super::{
    ConfigSummary, MetricsAggregator, MetricsSummary, ReceiptCoverage, RollingWindow,
    SubmissionStats, ThroughputSample, TransactionMetrics,
};
use crate::runner::BlockReceipt;

/// Collects transaction metrics during test execution.
#[derive(Debug)]
pub struct MetricsCollector {
    transactions: Vec<TransactionMetrics>,
    submitted_count: u64,
    failed_count: u64,
    reverted_count: u64,
    failure_reasons: HashMap<String, u64>,
    rolling: RollingWindow,
    flashblocks_rolling: RollingWindow,
    throughput_samples: Vec<ThroughputSample>,
    /// Fallback per-tx gas used for live throughput while the real receipt gas is
    /// still pending. Canonical gas arrives only in the end-of-run receipt pass, so
    /// without this the rolling GPS would read 0 for the whole run.
    estimated_gas: u64,
    /// Coverage of the end-of-run receipt pass, populated by [`Self::apply_receipts`].
    /// Signals whether the final gas/revert metrics are complete or partial.
    receipt_coverage: ReceiptCoverage,
}

impl MetricsCollector {
    /// Creates a new metrics collector.
    pub fn new() -> Self {
        Self {
            transactions: Vec::new(),
            submitted_count: 0,
            failed_count: 0,
            reverted_count: 0,
            failure_reasons: HashMap::new(),
            rolling: RollingWindow::new(),
            flashblocks_rolling: RollingWindow::new(),
            throughput_samples: Vec::new(),
            estimated_gas: 0,
            receipt_coverage: ReceiptCoverage::default(),
        }
    }

    /// Sets the estimated per-tx gas used for live throughput before receipts arrive.
    pub const fn set_estimated_gas(&mut self, estimated_gas: u64) {
        self.estimated_gas = estimated_gas;
    }

    /// Records a submitted transaction.
    pub const fn record_submitted(&mut self, _tx_hash: TxHash) {
        self.submitted_count += 1;
    }

    /// Records a transaction that landed in a polled block.
    ///
    /// Gas and revert status are unknown at landing time and stay at their defaults
    /// until [`Self::apply_receipts`] backfills them from the end-of-run receipt pass.
    pub fn record_confirmed(&mut self, metrics: TransactionMetrics) {
        debug!(
            tx_hash = %metrics.tx_hash,
            block_latency_ms = ?metrics.block_latency.map(|d| d.as_millis()),
            "tx landed"
        );
        let at = metrics.confirmed_at.unwrap_or_else(Instant::now);
        // Canonical gas is backfilled later by the receipt pass, so a freshly landed
        // tx reports gas_used == 0. Use the estimate for the live rolling window
        // (GPS, rate-limiter feedback); the final summary still uses real gas.
        let live_gas = if metrics.gas_used == 0 { self.estimated_gas } else { metrics.gas_used };
        if let Some(latency) = metrics.block_latency {
            self.rolling.push(live_gas, latency, at);
        } else {
            self.rolling.push_gas(live_gas, at);
        }
        self.transactions.push(metrics);
    }

    /// Records a flashblock observation from the WS stream.
    ///
    /// Called when a transaction is first seen in the flashblock websocket, using
    /// the actual WS observation time — not canonical block confirmation time.
    pub fn record_flashblock_observed(&mut self, latency: Duration, observed_at: Instant) {
        self.flashblocks_rolling.push_latency(latency, observed_at);
    }

    /// Backfills gas, effective gas price, and revert status onto landed transactions
    /// from the end-of-run canonical receipt pass.
    ///
    /// Receipts are keyed by transaction hash. Landed transactions without a matching
    /// receipt keep their default gas (0) and `reverted = false`. `blocks_total` and
    /// `blocks_failed` come from the fetch pass and, together with the per-transaction
    /// match counts computed here, populate the [`ReceiptCoverage`] surfaced in the
    /// summary so partial gas/revert data is visible to the user.
    pub fn apply_receipts(
        &mut self,
        receipts: &HashMap<TxHash, BlockReceipt>,
        blocks_total: usize,
        blocks_failed: usize,
    ) {
        let mut reverted = 0;
        let mut matched = 0;
        for tx in &mut self.transactions {
            let Some(receipt) = receipts.get(&tx.tx_hash) else {
                continue;
            };
            tx.gas_used = receipt.gas_used;
            tx.gas_price = receipt.effective_gas_price;
            tx.reverted = !receipt.success;
            if tx.reverted {
                reverted += 1;
            }
            matched += 1;
        }
        self.reverted_count = reverted;
        let total = self.transactions.len();
        let missing = total - matched;
        self.receipt_coverage = ReceiptCoverage {
            blocks_total: blocks_total as u64,
            blocks_failed: blocks_failed as u64,
            transactions_total: total as u64,
            transactions_matched: matched as u64,
            transactions_missing: missing as u64,
        };
        if missing > 0 || blocks_failed > 0 {
            warn!(
                matched,
                missing,
                total,
                blocks_failed,
                blocks_total,
                "end-of-run receipt pass incomplete: gas and revert metrics are partial"
            );
        } else {
            debug!(matched, total, reverted, "applied end-of-run receipts");
        }
    }

    /// Records a failed transaction with a categorized reason.
    pub fn record_failed(&mut self, _tx_hash: TxHash, reason: &str) {
        self.failed_count += 1;
        *self.failure_reasons.entry(reason.to_string()).or_insert(0) += 1;
    }

    /// Records multiple failures with the same reason.
    pub fn record_failures(&mut self, reason: &str, count: u64) {
        self.failed_count += count;
        *self.failure_reasons.entry(reason.to_string()).or_insert(0) += count;
    }

    /// Returns the number of confirmed transactions.
    pub const fn confirmed_count(&self) -> usize {
        self.transactions.len()
    }

    /// Returns the number of submitted transactions.
    pub const fn submitted_count(&self) -> u64 {
        self.submitted_count
    }

    /// Returns the number of failed transactions.
    pub const fn failed_count(&self) -> u64 {
        self.failed_count
    }

    /// Returns the number of confirmed transactions that reverted.
    pub const fn reverted_count(&self) -> u64 {
        self.reverted_count
    }

    /// Generates a summary of collected metrics.
    ///
    /// `wall_clock_duration` is used as a fallback when block timestamps are
    /// unavailable. TPS is normally derived from block time span.
    pub fn summarize(
        &self,
        wall_clock_duration: Duration,
        config: Option<ConfigSummary>,
    ) -> MetricsSummary {
        self.summarize_with_fresh_recipient_count(wall_clock_duration, config, None)
    }

    /// Generates a summary with optional fresh-recipient generation metadata.
    pub fn summarize_with_fresh_recipient_count(
        &self,
        wall_clock_duration: Duration,
        config: Option<ConfigSummary>,
        fresh_recipient_count: Option<u64>,
    ) -> MetricsSummary {
        let aggregator = MetricsAggregator::new(&self.transactions);
        aggregator.summarize(
            wall_clock_duration,
            SubmissionStats {
                submitted: self.submitted_count,
                failed: self.failed_count,
                failure_reasons: &self.failure_reasons,
            },
            &self.throughput_samples,
            config,
            self.receipt_coverage,
            fresh_recipient_count,
        )
    }

    /// Resets the collector for reuse.
    pub fn reset(&mut self) {
        self.transactions.clear();
        self.submitted_count = 0;
        self.failed_count = 0;
        self.reverted_count = 0;
        self.failure_reasons.clear();
        self.rolling = RollingWindow::new();
        self.flashblocks_rolling = RollingWindow::new();
        self.throughput_samples.clear();
        self.estimated_gas = 0;
        self.receipt_coverage = ReceiptCoverage::default();
    }

    /// Snapshots the current rolling TPS and GPS with elapsed time for timeseries output.
    pub fn sample_throughput(&mut self, elapsed: Duration) {
        let tps = self.rolling.tps();
        let gps = self.rolling.gps();
        if tps > 0.0 {
            self.throughput_samples.push(ThroughputSample {
                elapsed_secs: elapsed.as_secs_f64(),
                tps,
                gps,
            });
        }
    }

    /// Returns the rolling 30s TPS.
    pub fn rolling_tps(&mut self) -> f64 {
        self.rolling.tps()
    }

    /// Returns the rolling 30s GPS.
    pub fn rolling_gps(&mut self) -> f64 {
        self.rolling.gps()
    }

    /// Returns the rolling 30s (p50, p99) latency percentiles.
    pub fn rolling_p50_p99(&mut self) -> (std::time::Duration, std::time::Duration) {
        self.rolling.p50_p99()
    }

    /// Rolling 30s flashblocks (p50, p99).
    pub fn rolling_flashblocks_p50_p99(&mut self) -> (std::time::Duration, std::time::Duration) {
        self.flashblocks_rolling.p50_p99()
    }

    /// Returns the average gas used per confirmed transaction.
    ///
    /// Before the end-of-run receipt pass backfills canonical gas, landed txs report
    /// `gas_used == 0`; in that window this falls back to the configured estimate so
    /// the rate limiter keeps a non-zero feedback signal.
    pub fn avg_gas_used(&self) -> Option<u64> {
        if self.transactions.is_empty() {
            return None;
        }
        let total: u64 = self.transactions.iter().map(|t| t.gas_used).sum();
        let avg = total / self.transactions.len() as u64;
        if avg == 0 { (self.estimated_gas > 0).then_some(self.estimated_gas) } else { Some(avg) }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn landed(tx_hash: TxHash, block_number: u64) -> TransactionMetrics {
        TransactionMetrics::new(
            tx_hash,
            Some(Duration::from_millis(100)),
            None,
            0,
            0,
            Some(block_number),
        )
    }

    fn receipt(tx_hash: TxHash, gas_used: u64, success: bool) -> BlockReceipt {
        BlockReceipt {
            tx_hash,
            block_number: 1,
            gas_used,
            effective_gas_price: 1_000_000_000,
            success,
        }
    }

    #[test]
    fn apply_receipts_backfills_gas_and_revert() {
        let ok_hash = TxHash::repeat_byte(1);
        let bad_hash = TxHash::repeat_byte(2);
        let mut collector = MetricsCollector::new();
        collector.record_confirmed(landed(ok_hash, 5));
        collector.record_confirmed(landed(bad_hash, 5));

        let receipts = HashMap::from([
            (ok_hash, receipt(ok_hash, 21_000, true)),
            (bad_hash, receipt(bad_hash, 45_000, false)),
        ]);
        collector.apply_receipts(&receipts, 1, 0);

        let summary = collector.summarize(Duration::from_secs(1), None);
        assert_eq!(summary.gas.total_gas, 66_000, "gas backfilled from receipts");
        assert_eq!(summary.throughput.total_reverted, 1, "exactly one tx reverted");
        assert_eq!(collector.reverted_count(), 1, "reverted_count set by apply_receipts");
        assert!(summary.receipt_coverage.is_complete(), "all txs matched, no failed blocks");
        assert_eq!(summary.receipt_coverage.transactions_matched, 2, "both txs enriched");
        assert_eq!(summary.receipt_coverage.transactions_missing, 0, "no missing receipts");
    }

    #[test]
    fn apply_receipts_leaves_unmatched_tx_at_defaults() {
        let landed_hash = TxHash::repeat_byte(3);
        let unmatched_hash = TxHash::repeat_byte(4);
        let mut collector = MetricsCollector::new();
        collector.record_confirmed(landed(landed_hash, 7));
        collector.record_confirmed(landed(unmatched_hash, 7));

        let receipts = HashMap::from([(landed_hash, receipt(landed_hash, 30_000, true))]);
        collector.apply_receipts(&receipts, 1, 0);

        let summary = collector.summarize(Duration::from_secs(1), None);
        assert_eq!(summary.gas.total_gas, 30_000, "only matched tx contributes gas");
        assert_eq!(summary.throughput.total_reverted, 0, "no reverts");
        assert_eq!(collector.reverted_count(), 0, "unmatched tx stays non-reverted");
        let rc = summary.receipt_coverage;
        assert!(!rc.is_complete(), "one tx missing a receipt makes coverage incomplete");
        assert_eq!(rc.transactions_total, 2, "two confirmed txs");
        assert_eq!(rc.transactions_matched, 1, "exactly one tx enriched");
        assert_eq!(rc.transactions_missing, 1, "exactly one tx missing a receipt");
        assert_eq!(rc.blocks_failed, 0, "no block fetch failures in this case");
    }

    #[test]
    fn apply_receipts_records_failed_block_coverage() {
        let landed_hash = TxHash::repeat_byte(5);
        let mut collector = MetricsCollector::new();
        collector.record_confirmed(landed(landed_hash, 9));

        collector.apply_receipts(&HashMap::new(), 2, 2);

        let rc = collector.summarize(Duration::from_secs(1), None).receipt_coverage;
        assert!(!rc.is_complete(), "all blocks failed → coverage incomplete");
        assert_eq!(rc.blocks_total, 2, "two blocks attempted");
        assert_eq!(rc.blocks_failed, 2, "both block fetches failed");
        assert_eq!(rc.transactions_missing, 1, "the single confirmed tx got no receipt");
        assert_eq!(rc.transactions_matched, 0, "no txs enriched");
    }

    #[test]
    fn estimated_gas_drives_live_throughput_before_receipts() {
        let mut collector = MetricsCollector::new();
        collector.set_estimated_gas(21_000);

        // Landed txs report gas_used == 0 until the receipt pass runs.
        for i in 0..5u8 {
            collector.record_confirmed(landed(TxHash::repeat_byte(i), 100 + u64::from(i)));
        }

        assert!(collector.rolling_gps() > 0.0, "live GPS must use the estimate, not 0");
        assert_eq!(
            collector.avg_gas_used(),
            Some(21_000),
            "avg gas falls back to the estimate before receipts backfill real gas"
        );
    }

    #[test]
    fn live_throughput_is_zero_without_estimate() {
        let mut collector = MetricsCollector::new();
        collector.record_confirmed(landed(TxHash::repeat_byte(1), 100));

        assert_eq!(collector.rolling_gps(), 0.0, "no estimate set → GPS stays 0");
        assert_eq!(collector.avg_gas_used(), None, "no estimate and no real gas → None");
    }
}
