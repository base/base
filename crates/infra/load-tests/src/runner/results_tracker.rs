//! Result tracking for submitted transactions and inclusion observations.

use std::{
    collections::BTreeSet,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{
    Address, TxHash,
    map::{AddressMap, B256Map, B256Set},
};

use super::SubmitCohort;
use crate::metrics::TransactionMetrics;

/// A transaction entering submission.
#[derive(Debug, Clone, Copy)]
pub struct SentTransaction {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Sender address used for in-flight accounting.
    pub from: Address,
    /// Gas limit declared by the signed transaction.
    pub gas_limit: u64,
    /// Priority fee per gas the transaction was signed with.
    pub priority_fee: u128,
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
    /// Block header millisecond timestamp, when the observer read one. `None` for headers
    /// with only second-resolution timestamps.
    pub timestamp_ms: Option<u64>,
    /// Block base fee in wei, so a landed transaction's tip can be read off its receipt's
    /// effective gas price.
    pub base_fee: u128,
}

/// Load-test transactions matched while scanning one canonical block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockMatch {
    /// Number of matched transaction hashes.
    pub matched: u64,
    /// Sum of matched transactions' calibrated execution gas, released from in-flight
    /// accounting.
    pub included_gas: u128,
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

/// Canonical receipt data for a transaction. Used to backfill gas, effective gas price, and
/// revert status, and sampled during the run for gas calibration.
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

/// Lifecycle of the measured window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measurement {
    /// Before measurement: landings feed only the live display.
    Warmup,
    /// Measured submission is running. `end_block` is set for block-count runs.
    Measuring {
        /// Exclusive block number immediately before measured submission started.
        start_block: u64,
        /// Inclusive canonical block that ends measurement.
        end_block: Option<u64>,
    },
    /// The configured end block has been observed.
    Finished {
        /// Exclusive block number immediately before measured submission started.
        start_block: u64,
        /// Inclusive canonical block that ended measurement.
        end_block: u64,
    },
}

/// Tracks submitted transactions and turns inclusion observations into metrics.
///
/// Owned exclusively by the run coordinator; every other task reports through its inbox.
#[derive(Debug)]
pub struct ResultsTracker {
    pending: B256Map<PendingTransaction>,
    unreported_confirmations: Vec<TransactionMetrics>,
    sender_indices: AddressMap<usize>,
    in_flight: Vec<u64>,
    total_in_flight: u64,
    unconfirmed_gas: u128,
    confirmed_gas: u128,
    /// Block numbers in which at least one of our transactions landed, used to scope
    /// the end-of-run `eth_getBlockReceipts` pass to only relevant blocks.
    landed_blocks: BTreeSet<u64>,
    measurement: Measurement,
    measured_landed: B256Set,
    observed_gas_total: u128,
    observed_gas_count: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingTransaction {
    sender: usize,
    submit_time: Instant,
    /// Wall-clock time a submission RPC last accepted the transaction (registration time until
    /// then), comparable with block timestamps.
    submitted_at: SystemTime,
    measured: bool,
    gas_limit: u64,
    priority_fee: u128,
    estimated_gas: u64,
    cohort: SubmitCohort,
}

impl ResultsTracker {
    /// Creates a tracker for the given senders, indexed by their position.
    pub fn new(sender_addresses: &[Address]) -> Self {
        Self {
            pending: B256Map::default(),
            unreported_confirmations: Vec::new(),
            sender_indices: sender_addresses
                .iter()
                .enumerate()
                .map(|(index, address)| (*address, index))
                .collect(),
            in_flight: vec![0; sender_addresses.len()],
            total_in_flight: 0,
            unconfirmed_gas: 0,
            confirmed_gas: 0,
            landed_blocks: BTreeSet::new(),
            measurement: Measurement::Warmup,
            measured_landed: B256Set::default(),
            observed_gas_total: 0,
            observed_gas_count: 0,
        }
    }

    /// Returns the index of a tracked sender.
    pub fn sender_index(&self, address: &Address) -> Option<usize> {
        self.sender_indices.get(address).copied()
    }

    /// Records transactions a sender worker picked up at `submit_time`.
    pub fn sent_transactions(
        &mut self,
        transactions: Vec<SentTransaction>,
        submit_time: Instant,
        submitted_at: SystemTime,
    ) {
        let measuring = self.measurement != Measurement::Warmup;
        for transaction in transactions {
            let Some(sender) = self.sender_index(&transaction.from) else {
                continue;
            };
            let std::collections::hash_map::Entry::Vacant(entry) =
                self.pending.entry(transaction.tx_hash)
            else {
                continue;
            };
            entry.insert(PendingTransaction {
                sender,
                submit_time,
                submitted_at,
                measured: transaction.measured && measuring,
                gas_limit: transaction.gas_limit,
                priority_fee: transaction.priority_fee,
                estimated_gas: transaction.estimated_gas,
                cohort: transaction.cohort,
            });
            self.in_flight[sender] = self.in_flight[sender].saturating_add(1);
            self.total_in_flight = self.total_in_flight.saturating_add(1);
            self.unconfirmed_gas =
                self.unconfirmed_gas.saturating_add(u128::from(transaction.estimated_gas));
        }
    }

    /// Restamps a pending transaction with the start of the attempt that was accepted, so a
    /// retried batch is measured from the send that succeeded rather than its first one.
    pub fn accepted_transaction(&mut self, tx_hash: &TxHash, sent_at: SystemTime) {
        if let Some(pending) = self.pending.get_mut(tx_hash) {
            pending.submitted_at = sent_at;
        }
    }

    /// Tracks a pending transaction under the hash the node reported for it, restamping it with
    /// the start of the accepting attempt.
    pub fn rehash_transaction(&mut self, local: &TxHash, server: TxHash, sent_at: SystemTime) {
        if let Some(mut pending) = self.pending.remove(local) {
            pending.submitted_at = sent_at;
            self.pending.insert(server, pending);
        }
    }

    /// Removes a transaction the node definitively does not hold.
    pub fn discard_transaction(&mut self, tx_hash: &TxHash) -> bool {
        let Some(pending) = self.pending.remove(tx_hash) else {
            return false;
        };
        self.release(&pending);
        true
    }

    /// Records the transaction hashes observed in a canonical block.
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
        &mut self,
        block: BlockObservation,
        tx_hashes: &[TxHash],
    ) -> BlockMatch {
        self.observe_measurement_block(block.number);
        let mut block_match = BlockMatch::default();
        for tx_hash in tx_hashes {
            if let Some(estimated_gas) = self.land_if_pending(*tx_hash, &block) {
                block_match.matched = block_match.matched.saturating_add(1);
                block_match.included_gas =
                    block_match.included_gas.saturating_add(u128::from(estimated_gas));
            }
        }
        block_match
    }

    /// Stops tracking every pending transaction, returning the measured count and gas removed.
    pub fn drain_pending(&mut self) -> (u64, u128) {
        let inventory = self.measured_unconfirmed_inventory();
        self.pending.clear();
        self.in_flight.fill(0);
        self.total_in_flight = 0;
        self.unconfirmed_gas = 0;
        inventory
    }

    /// Drains confirmed metrics that have not yet been consumed by the runner.
    pub fn drain_confirmed_metrics(&mut self) -> std::vec::Drain<'_, TransactionMetrics> {
        self.unreported_confirmations.drain(..)
    }

    /// Returns the current pending transaction count.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Returns in-flight counts indexed by sender.
    pub fn in_flight_by_sender(&self) -> &[u64] {
        &self.in_flight
    }

    /// Returns the total in-flight count.
    pub const fn total_in_flight(&self) -> u64 {
        self.total_in_flight
    }

    /// Returns estimated execution gas not yet observed in a canonical block.
    pub const fn unconfirmed_gas(&self) -> u128 {
        self.unconfirmed_gas
    }

    /// Returns estimated measured gas observed since measurement began.
    pub const fn confirmed_gas(&self) -> u128 {
        self.confirmed_gas
    }

    /// Returns measured transactions and gas still awaiting canonical inclusion.
    pub fn measured_unconfirmed_inventory(&self) -> (u64, u128) {
        self.pending.values().filter(|pending| pending.measured).fold(
            (0u64, 0u128),
            |(count, gas), pending| {
                (count.saturating_add(1), gas.saturating_add(u128::from(pending.estimated_gas)))
            },
        )
    }

    /// Starts measurement. Transactions already accepted remain warmup transactions.
    pub fn begin_measurement(&mut self, start_block: u64, measurement_blocks: Option<u64>) {
        self.measurement = Measurement::Measuring {
            start_block,
            end_block: measurement_blocks.map(|count| start_block.saturating_add(count)),
        };
        self.unreported_confirmations.clear();
        self.landed_blocks.clear();
        self.measured_landed.clear();
        self.observed_gas_total = 0;
        self.observed_gas_count = 0;
        self.confirmed_gas = 0;
    }

    /// Returns the measurement lifecycle state.
    pub const fn measurement(&self) -> Measurement {
        self.measurement
    }

    /// Returns whether any measured transaction landed in `block_number`.
    pub fn landed_in_block(&self, block_number: u64) -> bool {
        self.landed_blocks.contains(&block_number)
    }

    /// Records canonical receipt gas for recently landed measured transactions.
    pub fn observe_live_receipts(&mut self, receipts: &[BlockReceipt]) {
        for receipt in receipts {
            if self.measured_landed.remove(&receipt.tx_hash) {
                self.observed_gas_total =
                    self.observed_gas_total.saturating_add(u128::from(receipt.gas_used));
                self.observed_gas_count = self.observed_gas_count.saturating_add(1);
            }
        }
    }

    /// Returns total canonical receipt gas and the count of measured transactions it covers.
    pub const fn observed_gas(&self) -> (u128, u64) {
        (self.observed_gas_total, self.observed_gas_count)
    }

    /// Returns the number of senders at or above the given in-flight limit.
    pub fn senders_at_limit(&self, limit: u64) -> usize {
        self.in_flight.iter().filter(|&&count| count >= limit).count()
    }

    /// Returns the sorted set of block numbers in which our transactions landed.
    ///
    /// Used to scope the end-of-run `eth_getBlockReceipts` pass to only the blocks
    /// that actually contained our transactions.
    pub fn landed_block_numbers(&self) -> Vec<u64> {
        self.landed_blocks.iter().copied().collect()
    }

    /// Returns whether the configured measurement block target has been observed.
    pub const fn measurement_finished(&self) -> bool {
        matches!(self.measurement, Measurement::Finished { .. })
    }

    /// Records that a canonical head was observed, independently of transaction hash scanning.
    pub const fn observe_measurement_block(&mut self, block_number: u64) {
        if let Measurement::Measuring { start_block, end_block: Some(end_block) } = self.measurement
            && block_number >= end_block
        {
            self.measurement = Measurement::Finished { start_block, end_block };
        }
    }

    /// Returns the configured measurement window boundaries.
    pub const fn measurement_window(&self) -> MeasurementWindow {
        match self.measurement {
            Measurement::Warmup => {
                MeasurementWindow { start_block: None, end_block: None, block_count: 0 }
            }
            Measurement::Measuring { start_block, end_block } => MeasurementWindow {
                start_block: Some(start_block),
                end_block,
                block_count: match end_block {
                    Some(end_block) => end_block.saturating_sub(start_block),
                    None => 0,
                },
            },
            Measurement::Finished { start_block, end_block } => MeasurementWindow {
                start_block: Some(start_block),
                end_block: Some(end_block),
                block_count: end_block.saturating_sub(start_block),
            },
        }
    }

    /// Milliseconds from a block's timestamp to a submission; negative when submitted before it.
    fn submit_offset_ms(submitted_at: SystemTime, block_timestamp_ms: u64) -> Option<i64> {
        let submitted_ms =
            i64::try_from(submitted_at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()?;
        Some(submitted_ms - i64::try_from(block_timestamp_ms).ok()?)
    }

    /// Records the first observation of `tx_hash` in a block, emitting its landing metrics.
    /// Idempotent: a tx is removed from `pending` on first landing, so later blocks
    /// containing the same hash are ignored.
    ///
    /// Returns the estimated gas when `tx_hash` was pending and is now settled.
    fn land_if_pending(&mut self, tx_hash: TxHash, block: &BlockObservation) -> Option<u64> {
        let pending = self.pending.remove(&tx_hash)?;
        self.release(&pending);

        // Measured txs always emit metrics. Pre-measurement txs emit only before
        // begin_measurement so live TUI/headroom can see inclusions; after
        // begin_measurement they only release in-flight (already handled above).
        if !pending.measured && self.measurement != Measurement::Warmup {
            return Some(pending.estimated_gas);
        }

        let block_latency = block.observed_at.checked_duration_since(pending.submit_time);
        let mut metrics = TransactionMetrics::new(tx_hash, block_latency, 0, 0, Some(block.number));
        metrics.cohort = pending.cohort.to_metric_label();
        metrics.gas_limit = pending.gas_limit;
        metrics.priority_fee = pending.priority_fee;
        metrics.base_fee = block.base_fee;
        metrics.confirmed_at = Some(block.observed_at);
        metrics.submit_offset_ms = block
            .timestamp_ms
            .and_then(|timestamp_ms| Self::submit_offset_ms(pending.submitted_at, timestamp_ms));
        self.unreported_confirmations.push(metrics);

        if pending.measured {
            self.landed_blocks.insert(block.number);
            self.measured_landed.insert(tx_hash);
            self.confirmed_gas =
                self.confirmed_gas.saturating_add(u128::from(pending.estimated_gas));
        }
        Some(pending.estimated_gas)
    }

    fn release(&mut self, pending: &PendingTransaction) {
        self.in_flight[pending.sender] = self.in_flight[pending.sender].saturating_sub(1);
        self.total_in_flight = self.total_in_flight.saturating_sub(1);
        self.unconfirmed_gas =
            self.unconfirmed_gas.saturating_sub(u128::from(pending.estimated_gas));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn addr() -> Address {
        Address::with_last_byte(1)
    }

    fn block_at(number: u64, observed_at: Instant) -> BlockObservation {
        BlockObservation { number, observed_at, timestamp_ms: None, base_fee: 0 }
    }

    fn sent(tx_hash: TxHash, from: Address, measured: bool) -> SentTransaction {
        SentTransaction {
            tx_hash,
            from,
            gas_limit: 200_000,
            priority_fee: 0,
            estimated_gas: 21_000,
            measured,
            cohort: SubmitCohort::Plain,
        }
    }

    fn send(tracker: &mut ResultsTracker, tx: SentTransaction) {
        tracker.sent_transactions(vec![tx], Instant::now(), SystemTime::now());
    }

    #[test]
    fn discard_pending_transaction_releases_in_flight_once() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(0xd0);
        let mut tracker = ResultsTracker::new(&[from]);

        send(&mut tracker, sent(tx_hash, from, true));
        assert_eq!(tracker.pending_count(), 1);
        assert_eq!(tracker.total_in_flight(), 1);
        assert_eq!(tracker.in_flight_by_sender(), &[1]);
        assert_eq!(tracker.unconfirmed_gas(), 21_000);

        assert!(tracker.discard_transaction(&tx_hash));
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.total_in_flight(), 0);
        assert_eq!(tracker.in_flight_by_sender(), &[0]);
        assert_eq!(tracker.unconfirmed_gas(), 0);

        assert!(!tracker.discard_transaction(&tx_hash), "second discard must be idempotent");
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn untracked_sender_is_ignored() {
        let mut tracker = ResultsTracker::new(&[addr()]);
        send(&mut tracker, sent(TxHash::repeat_byte(1), Address::with_last_byte(9), true));
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn records_submission_offset_from_block_timestamp() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(1);
        let mut tracker = ResultsTracker::new(&[from]);
        tracker.begin_measurement(6, Some(1));
        let submitted_at = UNIX_EPOCH + Duration::from_millis(4_000_000_000_000);
        tracker.sent_transactions(vec![sent(tx_hash, from, true)], Instant::now(), submitted_at);

        let block = BlockObservation {
            number: 7,
            observed_at: Instant::now(),
            timestamp_ms: Some(4_000_000_000_150),
            base_fee: 0,
        };
        tracker.on_new_block_hashes(block, &[tx_hash]);

        let offset = tracker.drain_confirmed_metrics().next().unwrap().submit_offset_ms;
        assert_eq!(offset, Some(-150));
    }

    #[test]
    fn acceptance_restamps_submission_time() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(1);
        let mut tracker = ResultsTracker::new(&[from]);
        tracker.begin_measurement(6, Some(1));
        send(&mut tracker, sent(tx_hash, from, true));
        // A retried send that succeeded 30 ms before the block timestamp, long after registration.
        let block_timestamp_ms = 4_000_000_000_000;
        let sent_at = UNIX_EPOCH + Duration::from_millis(block_timestamp_ms - 30);
        tracker.accepted_transaction(&tx_hash, sent_at);

        let block = BlockObservation {
            number: 7,
            observed_at: Instant::now(),
            timestamp_ms: Some(block_timestamp_ms),
            base_fee: 0,
        };
        tracker.on_new_block_hashes(block, &[tx_hash]);

        assert_eq!(tracker.drain_confirmed_metrics().next().unwrap().submit_offset_ms, Some(-30));
    }

    #[test]
    fn rehashed_transaction_lands_under_server_hash() {
        let from = addr();
        let local = TxHash::repeat_byte(1);
        let server = TxHash::repeat_byte(2);
        let mut tracker = ResultsTracker::new(&[from]);
        send(&mut tracker, sent(local, from, true));

        tracker.rehash_transaction(&local, server, SystemTime::now());
        assert_eq!(tracker.on_new_block_hashes(block_at(3, Instant::now()), &[local]).matched, 0);
        assert_eq!(tracker.on_new_block_hashes(block_at(4, Instant::now()), &[server]).matched, 1);
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn confirms_pending_transaction_from_block_hashes() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(1);
        let mut tracker = ResultsTracker::new(&[from]);
        tracker.begin_measurement(6, Some(1));

        send(&mut tracker, sent(tx_hash, from, true));
        assert_eq!(tracker.unconfirmed_gas(), 21_000);
        let observed_at = Instant::now() + Duration::from_millis(250);
        tracker.on_new_block_hashes(block_at(7, observed_at), &[tx_hash]);

        let metrics: Vec<_> = tracker.drain_confirmed_metrics().collect();
        assert_eq!(metrics.len(), 1, "landed tx should produce exactly one metric");
        assert_eq!(metrics[0].tx_hash, tx_hash);
        assert_eq!(metrics[0].block_number, Some(7), "block number from polled block");
        assert!(metrics[0].block_latency.is_some(), "landing latency must be recorded");
        assert_eq!(metrics[0].gas_used, 0, "gas backfilled later by receipt pass");
        assert_eq!(metrics[0].gas_limit, 200_000, "declared limit is known at landing time");
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
        assert_eq!(tracker.observed_gas(), (42_000, 1));
    }

    #[test]
    fn second_block_with_same_hash_is_ignored() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(7);
        let mut tracker = ResultsTracker::new(&[from]);
        tracker.begin_measurement(10, Some(2));

        send(&mut tracker, sent(tx_hash, from, true));
        let now = Instant::now();
        tracker.on_new_block_hashes(block_at(11, now + Duration::from_millis(100)), &[tx_hash]);
        let duplicate =
            tracker.on_new_block_hashes(block_at(12, now + Duration::from_millis(300)), &[tx_hash]);

        assert_eq!(duplicate, BlockMatch::default(), "settled tx must not match again");
        let metrics: Vec<_> = tracker.drain_confirmed_metrics().collect();
        assert_eq!(metrics.len(), 1, "tx should land exactly once despite reappearing");
        assert_eq!(metrics[0].block_number, Some(11), "first-seen block wins");
        assert_eq!(tracker.landed_block_numbers(), vec![11], "only first block tracked");
        assert_eq!(tracker.total_in_flight(), 0, "duplicate block should not underflow");
    }

    #[test]
    fn drain_pending_counts_measured_transactions_and_releases_in_flight() {
        let from = addr();
        let mut tracker = ResultsTracker::new(&[from]);
        send(&mut tracker, sent(TxHash::repeat_byte(0xe1), from, false));
        tracker.begin_measurement(0, None);
        send(&mut tracker, sent(TxHash::repeat_byte(0xe0), from, true));
        assert_eq!(tracker.pending_count(), 2);

        assert_eq!(tracker.drain_pending(), (1, 21_000), "only measured inventory is reported");
        assert_eq!(tracker.pending_count(), 0);
        assert_eq!(tracker.total_in_flight(), 0);
        assert_eq!(tracker.in_flight_by_sender(), &[0]);
        assert_eq!(tracker.unconfirmed_gas(), 0);
    }

    #[test]
    fn warmup_landing_emits_metrics_before_measurement() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(0xe2);
        let mut tracker = ResultsTracker::new(&[from]);

        send(&mut tracker, sent(tx_hash, from, false));
        tracker.on_new_block_hashes(block_at(3, Instant::now()), &[tx_hash]);
        assert_eq!(tracker.total_in_flight(), 0);
        assert_eq!(
            tracker.drain_confirmed_metrics().count(),
            1,
            "warmup landing should emit live confirmation metrics"
        );
        assert!(tracker.landed_block_numbers().is_empty(), "warmup must not scope receipt pass");
    }

    #[test]
    fn warmup_landing_after_measurement_does_not_pollute_metrics() {
        let from = addr();
        let tx_hash = TxHash::repeat_byte(0xe3);
        let mut tracker = ResultsTracker::new(&[from]);

        send(&mut tracker, sent(tx_hash, from, false));
        tracker.begin_measurement(3, Some(1));
        tracker.on_new_block_hashes(block_at(4, Instant::now()), &[tx_hash]);

        assert_eq!(tracker.total_in_flight(), 0);
        assert_eq!(
            tracker.drain_confirmed_metrics().count(),
            0,
            "warmup landing after measurement must not enter the measured summary"
        );
    }

    #[test]
    fn measurement_window_counts_exact_target_with_empty_blocks() {
        let mut tracker = ResultsTracker::new(&[]);
        tracker.observe_measurement_block(500);
        assert_eq!(tracker.measurement(), Measurement::Warmup, "warmup ignores heads");
        tracker.begin_measurement(100, Some(4));

        for number in 101..=103 {
            tracker.on_new_block_hashes(block_at(number, Instant::now()), &[]);
        }
        assert!(!tracker.measurement_finished());

        tracker.on_new_block_hashes(block_at(104, Instant::now()), &[]);

        assert_eq!(
            tracker.measurement_window(),
            MeasurementWindow { start_block: Some(100), end_block: Some(104), block_count: 4 }
        );
        assert!(tracker.measurement_finished());
    }

    #[test]
    fn measurement_window_handles_skipped_height_observation_order() {
        let mut tracker = ResultsTracker::new(&[]);
        tracker.begin_measurement(200, Some(3));

        // Newest block arrives first; skipped heights can be recovered later.
        tracker.on_new_block_hashes(block_at(204, Instant::now()), &[]);
        assert!(tracker.measurement_finished());
        tracker.on_new_block_hashes(block_at(202, Instant::now()), &[]);
        tracker.on_new_block_hashes(block_at(203, Instant::now()), &[]);

        assert_eq!(
            tracker.measurement_window(),
            MeasurementWindow { start_block: Some(200), end_block: Some(203), block_count: 3 }
        );
    }

    #[test]
    fn unbounded_measurement_never_finishes() {
        let mut tracker = ResultsTracker::new(&[]);
        tracker.begin_measurement(10, None);
        tracker.observe_measurement_block(u64::MAX);
        assert!(!tracker.measurement_finished());
        assert_eq!(
            tracker.measurement_window(),
            MeasurementWindow { start_block: Some(10), end_block: None, block_count: 0 }
        );
    }
}
