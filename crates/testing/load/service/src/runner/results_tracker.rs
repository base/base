//! Result tracking for submitted transactions and inclusion observations.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash};
use parking_lot::RwLock;

use super::SubmitCohort;
use crate::metrics::TransactionMetrics;

/// Maximum terminal batch timestamps retained after refill timeouts.
const MAX_COMPLETED_BATCH_CACHE_SIZE: usize = 4_096;

/// A transaction accepted by a submission RPC.
#[derive(Debug, Clone, Copy)]
pub struct SentTransaction {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Sender address used for in-flight accounting.
    pub from: Address,
    /// Calibrated execution gas used for pacing.
    pub estimated_gas: u64,
    /// Whether this transaction belongs to the measured cohort.
    pub measured: bool,
    /// Submission cohort this transaction was routed through.
    pub cohort: SubmitCohort,
}

/// A block observed by the block watcher.
#[derive(Debug, Clone, Copy)]
pub struct BlockObservation {
    /// Canonical block number.
    pub number: u64,
    /// Local time when the load-test process observed the block. Used as the
    /// landing time for transactions first seen in this block.
    pub observed_at: Instant,
}

/// Load-test transactions matched while scanning one canonical block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockMatch {
    /// Number of matched transaction hashes.
    pub matched: u64,
    /// Sum of matched transactions' calibrated execution gas.
    pub included_gas: u128,
    /// Calibrated gas newly released from in-flight accounting.
    pub released_gas: u128,
}

/// Canonical block boundaries of the measured submission window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MeasurementWindow {
    /// Exclusive block number immediately before measured submission started.
    pub start_block: Option<u64>,
    /// Inclusive canonical block where measurement ends.
    pub end_block: Option<u64>,
    /// Number of canonical blocks in the window (`end_block - start_block`).
    pub block_count: u64,
}

/// Canonical receipt data for a transaction, fetched in a single batch pass at the
/// end of the load test (not during the run). Used to backfill gas, effective gas
/// price, and revert status.
#[derive(Debug, Clone, Copy)]
pub struct BlockReceipt {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Canonical block number containing the transaction.
    pub block_number: u64,
    /// Gas consumed by the transaction execution.
    pub gas_used: u64,
    /// Effective gas price in wei.
    pub effective_gas_price: u128,
    /// Whether the transaction executed successfully (`false` = reverted).
    pub success: bool,
}

/// Tracks submitted transactions and turns inclusion observations into metrics.
#[derive(Debug, Clone)]
pub struct ResultsTracker {
    inner: Arc<RwLock<ResultsTrackerInner>>,
}

#[derive(Debug)]
struct ResultsTrackerInner {
    pending: HashMap<TxHash, PendingTransaction>,

    unreported_confirmations: VecDeque<TransactionMetrics>,

    in_flight_per_sender: HashMap<Address, u64>,
    total_in_flight: u64,
    unconfirmed_gas: u128,
    confirmed_gas: u128,
    /// Block numbers in which at least one of our transactions landed, used to scope
    /// the end-of-run `eth_getBlockReceipts` pass to only relevant blocks.
    landed_blocks: BTreeSet<u64>,
    measurement_started: bool,
    measurement_start_block: Option<u64>,
    measurement_end_block: Option<u64>,
    measurement_target_count: Option<u64>,
    measurement_finished: bool,
    measured_landed: HashSet<TxHash>,
    completed_batches: HashMap<u64, Instant>,
    pending_refills: VecDeque<PendingRefill>,
    completed_refill_lags: VecDeque<Duration>,
    observed_gas_total: u128,
    observed_gas_count: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingTransaction {
    from: Address,
    submit_time: Instant,

    measured: bool,
    estimated_gas: u64,
    cohort: SubmitCohort,
}

#[derive(Debug)]
struct PendingRefill {
    batch_ids: Vec<u64>,
    started_at: Instant,
}

impl ResultsTrackerInner {
    fn resolve_pending_refills(&mut self) {
        for index in (0..self.pending_refills.len()).rev() {
            let refill = &self.pending_refills[index];
            if refill.batch_ids.iter().any(|id| !self.completed_batches.contains_key(id)) {
                continue;
            }

            let latest = refill
                .batch_ids
                .iter()
                .filter_map(|id| self.completed_batches.remove(id))
                .max()
                .expect("pending refill contains at least one batch");
            let refill = self.pending_refills.swap_remove_back(index).expect("index is in bounds");
            self.completed_refill_lags
                .push_back(latest.saturating_duration_since(refill.started_at));
        }
    }
}

impl ResultsTracker {
    /// Creates a new tracker for the given sender addresses.
    pub fn new(sender_addresses: &[Address]) -> Self {
        let in_flight_per_sender =
            sender_addresses.iter().copied().map(|address| (address, 0)).collect();
        Self {
            inner: Arc::new(RwLock::new(ResultsTrackerInner {
                pending: HashMap::new(),
                unreported_confirmations: VecDeque::new(),
                in_flight_per_sender,
                total_in_flight: 0,
                unconfirmed_gas: 0,
                confirmed_gas: 0,
                landed_blocks: BTreeSet::new(),
                measurement_started: false,
                measurement_start_block: None,
                measurement_end_block: None,
                measurement_target_count: None,
                measurement_finished: false,
                measured_landed: HashSet::new(),
                completed_batches: HashMap::new(),
                pending_refills: VecDeque::new(),
                completed_refill_lags: VecDeque::new(),
                observed_gas_total: 0,
                observed_gas_count: 0,
            })),
        }
    }

    /// Records signed transactions entering submission.
    pub fn sent_transactions(&self, transactions: Vec<SentTransaction>) {
        let submit_time = Instant::now();
        let mut inner = self.inner.write();

        for transaction in transactions {
            if inner.pending.contains_key(&transaction.tx_hash) {
                continue;
            }

            let measured = transaction.measured && inner.measurement_started;
            inner.pending.insert(
                transaction.tx_hash,
                PendingTransaction {
                    from: transaction.from,
                    submit_time,
                    measured,
                    estimated_gas: transaction.estimated_gas,
                    cohort: transaction.cohort,
                },
            );
            inner
                .in_flight_per_sender
                .entry(transaction.from)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
            inner.total_in_flight = inner.total_in_flight.saturating_add(1);
            inner.unconfirmed_gas =
                inner.unconfirmed_gas.saturating_add(u128::from(transaction.estimated_gas));
        }
        drop(inner);
    }

    /// Removes a transaction that the submission RPC explicitly rejected.
    pub fn discard_transaction(&self, tx_hash: TxHash) -> bool {
        let mut inner = self.inner.write();
        let Some(pending) = inner.pending.remove(&tx_hash) else {
            return false;
        };
        inner.decrement_in_flight(&pending.from, pending.estimated_gas);
        true
    }

    /// Records the transaction hashes observed in a newly polled canonical block.
    ///
    /// This is the in-run landing detector: the first time one of our pending
    /// transactions is seen in a block's transaction list, its landing latency
    /// (submit -> first-seen, which includes the block poll + scan cost) is recorded,
    /// its block number is captured, and a [`TransactionMetrics`] entry is emitted.
    /// Gas, effective gas price, and revert status are left at defaults here and
    /// backfilled later by the end-of-run receipt pass.
    ///
    /// Returns the count and estimated gas of matching pending submissions.
    pub fn on_new_block_hashes(
        &self,
        block: BlockObservation,
        tx_hashes: Vec<TxHash>,
    ) -> BlockMatch {
        let mut inner = self.inner.write();
        inner.observe_measurement_block(block.number);
        let mut block_match = BlockMatch::default();
        for tx_hash in tx_hashes {
            if let Some(estimated_gas) = inner.land_if_pending(tx_hash, &block) {
                block_match.matched = block_match.matched.saturating_add(1);
                block_match.included_gas =
                    block_match.included_gas.saturating_add(u128::from(estimated_gas));
                block_match.released_gas =
                    block_match.released_gas.saturating_add(u128::from(estimated_gas));
            }
        }
        block_match
    }

    /// Expires submitted transactions that were not observed in a canonical block.
    ///
    /// Removes pending entries older than `max_age` and returns the number of
    /// expired measured transactions.
    pub fn expire_pending(&self, max_age: Duration) -> u64 {
        let now = Instant::now();
        let mut inner = self.inner.write();
        let expired: Vec<_> = inner
            .pending
            .iter()
            .filter_map(|(tx_hash, pending)| {
                (now.duration_since(pending.submit_time) > max_age).then_some(*tx_hash)
            })
            .collect();

        let mut unconfirmed_count = 0u64;
        for tx_hash in expired {
            if let Some(pending) = inner.pending.remove(&tx_hash) {
                inner.decrement_in_flight(&pending.from, pending.estimated_gas);
                if pending.measured {
                    unconfirmed_count += 1;
                }
            }
        }

        unconfirmed_count
    }

    /// Drains confirmed metrics that have not yet been consumed by the runner.
    pub fn drain_confirmed_metrics(&self) -> Vec<TransactionMetrics> {
        let mut inner = self.inner.write();
        inner.unreported_confirmations.drain(..).collect()
    }

    /// Returns the current pending transaction count.
    pub fn pending_count(&self) -> usize {
        self.inner.read().pending.len()
    }

    /// Returns the in-flight count for a specific sender.
    pub fn in_flight_for(&self, address: &Address) -> u64 {
        self.inner.read().in_flight_per_sender.get(address).copied().unwrap_or(0)
    }

    /// Returns the total in-flight count.
    pub fn total_in_flight(&self) -> u64 {
        self.inner.read().total_in_flight
    }

    /// Returns estimated execution gas not yet observed in a canonical block.
    pub fn unconfirmed_gas(&self) -> u128 {
        self.inner.read().unconfirmed_gas
    }

    /// Returns estimated measured gas observed since measurement began.
    pub fn confirmed_gas(&self) -> u128 {
        self.inner.read().confirmed_gas
    }

    /// Returns measured transactions and gas still awaiting canonical inclusion.
    pub fn measured_unconfirmed_inventory(&self) -> (u64, u128) {
        self.inner.read().pending.values().filter(|pending| pending.measured).fold(
            (0u64, 0u128),
            |(count, gas), pending| {
                (count.saturating_add(1), gas.saturating_add(u128::from(pending.estimated_gas)))
            },
        )
    }

    /// Records terminal completion of one submission batch.
    pub fn record_batch_completed(&self, id: u64, completed_at: Instant) {
        let mut inner = self.inner.write();
        if inner.completed_batches.len() >= MAX_COMPLETED_BATCH_CACHE_SIZE {
            let oldest_retained = id.saturating_sub((MAX_COMPLETED_BATCH_CACHE_SIZE / 2) as u64);
            inner.completed_batches.retain(|batch_id, _| *batch_id >= oldest_retained);
        }
        inner.completed_batches.insert(id, completed_at);
        inner.resolve_pending_refills();
    }

    /// Registers batches whose acknowledgement completed after the synchronous refill budget.
    pub fn register_pending_refill(&self, batch_ids: Vec<u64>, started_at: Instant) {
        if batch_ids.is_empty() {
            return;
        }
        let mut inner = self.inner.write();
        if inner.pending_refills.len() >= MAX_COMPLETED_BATCH_CACHE_SIZE {
            inner.pending_refills.pop_front();
        }
        inner.pending_refills.push_back(PendingRefill { batch_ids, started_at });
        inner.resolve_pending_refills();
    }

    /// Drains exact acknowledgement latencies for refills that completed asynchronously.
    pub fn drain_completed_refill_lags(&self) -> Vec<Duration> {
        self.inner.write().completed_refill_lags.drain(..).collect()
    }

    /// Removes completed entries for `batch_ids`, returning the latest completion time.
    pub fn take_completed_batches(&self, batch_ids: &[u64]) -> Option<Instant> {
        let mut inner = self.inner.write();
        if batch_ids.iter().any(|id| !inner.completed_batches.contains_key(id)) {
            return None;
        }
        let mut latest = None;
        for id in batch_ids {
            let completed_at = inner.completed_batches.remove(id).expect("presence checked above");
            latest =
                Some(latest.map_or(completed_at, |current: Instant| current.max(completed_at)));
        }
        latest
    }

    /// Starts measurement. Transactions already accepted remain warmup transactions.
    pub fn begin_measurement(&self, start_block: u64, measurement_blocks: Option<u64>) {
        let mut inner = self.inner.write();
        inner.measurement_started = true;
        inner.measurement_start_block = Some(start_block);
        inner.measurement_target_count = measurement_blocks;
        inner.measurement_end_block =
            measurement_blocks.map(|count| start_block.saturating_add(count));
        inner.measurement_finished = false;
        inner.unreported_confirmations.clear();
        inner.landed_blocks.clear();
        inner.measured_landed.clear();
        inner.observed_gas_total = 0;
        inner.observed_gas_count = 0;
        inner.confirmed_gas = 0;
    }

    /// Returns whether this block contains a measured transaction awaiting canonical inclusion.
    pub fn has_measured_pending(&self, tx_hashes: &[TxHash]) -> bool {
        let inner = self.inner.read();
        tx_hashes.iter().any(|hash| inner.pending.get(hash).is_some_and(|pending| pending.measured))
    }

    /// Records canonical receipt gas for recently landed measured transactions.
    pub fn observe_live_receipts(&self, receipts: &[BlockReceipt]) {
        let mut inner = self.inner.write();
        for receipt in receipts {
            if inner.measured_landed.remove(&receipt.tx_hash) {
                inner.observed_gas_total =
                    inner.observed_gas_total.saturating_add(u128::from(receipt.gas_used));
                inner.observed_gas_count = inner.observed_gas_count.saturating_add(1);
            }
        }
    }

    /// Returns average canonical gas for measured transactions observed during this run.
    pub fn observed_avg_gas(&self) -> Option<u64> {
        let inner = self.inner.read();
        if inner.observed_gas_count == 0 {
            return None;
        }
        u64::try_from(inner.observed_gas_total / u128::from(inner.observed_gas_count)).ok()
    }

    /// Returns the number of senders at or above the given in-flight limit.
    pub fn senders_at_limit(&self, limit: u64) -> usize {
        self.inner.read().in_flight_per_sender.values().filter(|&&count| count >= limit).count()
    }

    /// Returns the sorted set of block numbers in which our transactions landed.
    ///
    /// Used to scope the end-of-run `eth_getBlockReceipts` pass to only the blocks
    /// that actually contained our transactions.
    pub fn landed_block_numbers(&self) -> Vec<u64> {
        self.inner.read().landed_blocks.iter().copied().collect()
    }

    /// Returns whether the configured measurement block target has been observed.
    pub fn measurement_finished(&self) -> bool {
        self.inner.read().measurement_finished
    }

    /// Returns the configured measurement window boundaries.
    pub fn measurement_window(&self) -> MeasurementWindow {
        let inner = self.inner.read();
        let block_count = inner
            .measurement_start_block
            .zip(inner.measurement_end_block)
            .map_or(0, |(start, end)| end.saturating_sub(start));
        MeasurementWindow {
            start_block: inner.measurement_start_block,
            end_block: inner.measurement_end_block,
            block_count,
        }
    }
}

impl ResultsTrackerInner {
    /// Records the first observation of `tx_hash` in a polled block, emitting its
    /// landing metrics. Idempotent: a tx is removed from `pending` on first landing,
    /// so later blocks containing the same hash are ignored.
    ///
    /// Returns the estimated gas when `tx_hash` was pending and is now settled.
    fn land_if_pending(&mut self, tx_hash: TxHash, block: &BlockObservation) -> Option<u64> {
        let pending = self.pending.remove(&tx_hash)?;

        let block_latency = block.observed_at.checked_duration_since(pending.submit_time);

        self.decrement_in_flight(&pending.from, pending.estimated_gas);

        // Measured txs always emit metrics. Pre-measurement txs emit only before
        // begin_measurement so live TUI/headroom can see inclusions; after
        // begin_measurement they only release in-flight (already handled above).
        let emit_metrics = pending.measured || !self.measurement_started;
        if !emit_metrics {
            return Some(pending.estimated_gas);
        }

        let mut metrics = TransactionMetrics::new(tx_hash, block_latency, 0, 0, Some(block.number));
        metrics.cohort = pending.cohort.to_metric_label();
        metrics.confirmed_at = Some(block.observed_at);
        self.unreported_confirmations.push_back(metrics);

        if pending.measured {
            self.landed_blocks.insert(block.number);
            self.measured_landed.insert(tx_hash);
            self.confirmed_gas =
                self.confirmed_gas.saturating_add(u128::from(pending.estimated_gas));
        }
        Some(pending.estimated_gas)
    }

    fn decrement_in_flight(&mut self, from: &Address, estimated_gas: u64) {
        if let Some(count) = self.in_flight_per_sender.get_mut(from) {
            *count = count.saturating_sub(1);
        }
        self.total_in_flight = self.total_in_flight.saturating_sub(1);
        self.unconfirmed_gas = self.unconfirmed_gas.saturating_sub(u128::from(estimated_gas));
    }

    const fn observe_measurement_block(&mut self, observed_block: u64) {
        if !self.measurement_started || self.measurement_finished {
            return;
        }
        if let Some(target_end_block) = self.measurement_end_block
            && observed_block >= target_end_block
        {
            self.measurement_finished = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    fn block_at(number: u64, observed_at: Instant) -> BlockObservation {
        BlockObservation { number, observed_at }
    }

    fn sent(tx_hash: TxHash, from: Address, measured: bool) -> SentTransaction {
        SentTransaction {
            tx_hash,
            from,
            estimated_gas: 21_000,
            measured,
            cohort: SubmitCohort::Plain,
        }
    }

    #[test]
    fn discard_pending_transaction_releases_in_flight_once() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(0xd0);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![sent(tx_hash, from, true)]);
        assert_eq!(tracker.pending_count(), 1);
        assert_eq!(tracker.total_in_flight(), 1);
        assert_eq!(tracker.unconfirmed_gas(), 21_000);

        assert!(tracker.discard_transaction(tx_hash));
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.total_in_flight(), 0);
        assert_eq!(tracker.unconfirmed_gas(), 0);

        assert!(!tracker.discard_transaction(tx_hash), "second discard must be idempotent");
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn confirms_pending_transaction_from_block_hashes() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(1);
        let tracker = ResultsTracker::new(&[from]);
        tracker.begin_measurement(6, Some(1));

        tracker.sent_transactions(vec![sent(tx_hash, from, true)]);
        assert_eq!(tracker.unconfirmed_gas(), 21_000);
        let observed_at = Instant::now() + Duration::from_millis(250);
        tracker.on_new_block_hashes(block_at(7, observed_at), vec![tx_hash]);

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1, "landed tx should produce exactly one metric");
        assert_eq!(metrics[0].tx_hash, tx_hash);
        assert_eq!(metrics[0].block_number, Some(7), "block number from polled block");
        assert!(metrics[0].block_latency.is_some(), "landing latency must be recorded");
        assert_eq!(metrics[0].gas_used, 0, "gas backfilled later by receipt pass");
        assert!(!metrics[0].reverted, "revert backfilled later by receipt pass");
        assert_eq!(tracker.landed_block_numbers(), vec![7], "block 7 tracked for receipt pass");
        assert_eq!(tracker.total_in_flight(), 0, "landing releases in-flight slot");
        assert_eq!(tracker.unconfirmed_gas(), 0);
        assert_eq!(tracker.confirmed_gas(), 21_000);

        tracker.observe_live_receipts(&[BlockReceipt {
            tx_hash,
            block_number: 7,
            gas_used: 42_000,
            effective_gas_price: 1,
            success: true,
        }]);
        assert_eq!(tracker.observed_avg_gas(), Some(42_000));
    }

    #[test]
    fn second_block_with_same_hash_is_ignored() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(7);
        let tracker = ResultsTracker::new(&[from]);
        tracker.begin_measurement(10, Some(2));

        tracker.sent_transactions(vec![sent(tx_hash, from, true)]);
        let now = Instant::now();
        tracker.on_new_block_hashes(block_at(11, now + Duration::from_millis(100)), vec![tx_hash]);
        tracker.on_new_block_hashes(block_at(12, now + Duration::from_millis(300)), vec![tx_hash]);

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1, "tx should land exactly once despite reappearing");
        assert_eq!(metrics[0].block_number, Some(11), "first-seen block wins");
        assert_eq!(tracker.landed_block_numbers(), vec![11], "only first block tracked");
    }

    #[test]
    fn expired_setup_transaction_is_not_a_measured_failure() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(0xe1);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![sent(tx_hash, from, false)]);
        tracker.begin_measurement(0, None);

        assert_eq!(tracker.expire_pending(Duration::ZERO), 0);
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn warmup_landing_emits_metrics_before_measurement() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(0xe2);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![sent(tx_hash, from, false)]);
        assert_eq!(tracker.total_in_flight(), 1);

        tracker.on_new_block_hashes(block_at(3, Instant::now()), vec![tx_hash]);
        assert_eq!(tracker.total_in_flight(), 0);
        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1, "warmup landing should emit live confirmation metrics");
        assert!(tracker.landed_block_numbers().is_empty(), "warmup must not scope receipt pass");
    }

    #[test]
    fn warmup_landing_after_measurement_does_not_pollute_metrics() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(0xe3);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![sent(tx_hash, from, false)]);
        tracker.begin_measurement(3, Some(1));
        tracker.on_new_block_hashes(block_at(4, Instant::now()), vec![tx_hash]);

        assert_eq!(tracker.total_in_flight(), 0);
        assert!(
            tracker.drain_confirmed_metrics().is_empty(),
            "warmup landing after measurement must not enter the measured summary"
        );
    }

    #[test]
    fn preserves_late_refill_acknowledgement_latency() {
        let tracker = ResultsTracker::new(&[]);
        let started_at = Instant::now();
        tracker.register_pending_refill(vec![10, 11], started_at);

        tracker.record_batch_completed(10, started_at + Duration::from_millis(120));
        assert!(tracker.drain_completed_refill_lags().is_empty());
        tracker.record_batch_completed(11, started_at + Duration::from_millis(145));

        assert_eq!(tracker.drain_completed_refill_lags(), vec![Duration::from_millis(145)]);
    }

    #[test]
    fn measurement_window_counts_exact_target_with_empty_blocks() {
        let tracker = ResultsTracker::new(&[]);
        tracker.begin_measurement(100, Some(4));

        tracker.on_new_block_hashes(block_at(101, Instant::now()), Vec::new());
        tracker.on_new_block_hashes(block_at(102, Instant::now()), Vec::new());
        tracker.on_new_block_hashes(block_at(103, Instant::now()), Vec::new());
        assert!(!tracker.measurement_finished());

        tracker.on_new_block_hashes(block_at(104, Instant::now()), Vec::new());

        assert_eq!(
            tracker.measurement_window(),
            MeasurementWindow { start_block: Some(100), end_block: Some(104), block_count: 4 }
        );
        assert!(tracker.measurement_finished());
    }

    #[test]
    fn measurement_window_handles_skipped_height_observation_order() {
        let tracker = ResultsTracker::new(&[]);
        tracker.begin_measurement(200, Some(3));

        // Newest block arrives first; skipped heights can be recovered later.
        tracker.on_new_block_hashes(block_at(204, Instant::now()), Vec::new());
        assert!(tracker.measurement_finished());
        tracker.on_new_block_hashes(block_at(202, Instant::now()), Vec::new());
        tracker.on_new_block_hashes(block_at(203, Instant::now()), Vec::new());

        assert_eq!(
            tracker.measurement_window(),
            MeasurementWindow { start_block: Some(200), end_block: Some(203), block_count: 3 }
        );
    }
}
