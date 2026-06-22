//! Result tracking for submitted transactions and inclusion observations.

use std::{
    collections::{BTreeSet, HashMap, VecDeque, hash_map::Entry},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash};
use parking_lot::RwLock;

use crate::metrics::TransactionMetrics;

/// Maximum flashblock entries retained from recent stream events.
const MAX_FLASHBLOCK_CACHE_SIZE: usize = 50_000;

/// A transaction accepted by a submission RPC.
#[derive(Debug, Clone, Copy)]
pub struct SentTransaction {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Sender address used for in-flight accounting.
    pub from: Address,
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

/// Transaction data observed from the builder flashblocks broadcast stream.
#[derive(Debug, Clone, Copy)]
pub struct FlashblockInclusion {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// When the load test client received the flashblock transaction notification.
    pub included_at: Instant,
}

/// Tracks submitted transactions and turns inclusion observations into metrics.
#[derive(Debug, Clone)]
pub struct ResultsTracker {
    inner: Arc<RwLock<ResultsTrackerInner>>,
}

#[derive(Debug)]
struct ResultsTrackerInner {
    pending: HashMap<TxHash, PendingTransaction>,
    flashblocks: HashMap<TxHash, Instant>,
    flashblock_eviction_queue: VecDeque<TxHash>,
    unreported_confirmations: VecDeque<TransactionMetrics>,
    in_flight_per_sender: HashMap<Address, u64>,
    total_in_flight: u64,
    /// Block numbers in which at least one of our transactions landed, used to scope
    /// the end-of-run `eth_getBlockReceipts` pass to only relevant blocks.
    landed_blocks: BTreeSet<u64>,
}

#[derive(Debug, Clone, Copy)]
struct PendingTransaction {
    from: Address,
    submit_time: Instant,
    /// Whether in-flight accounting was already released (e.g. by flashblock confirmation).
    in_flight_released: bool,
}

impl ResultsTracker {
    /// Creates a new tracker for the given sender addresses.
    pub fn new(sender_addresses: &[Address]) -> Self {
        let in_flight_per_sender =
            sender_addresses.iter().copied().map(|address| (address, 0)).collect();
        Self {
            inner: Arc::new(RwLock::new(ResultsTrackerInner {
                pending: HashMap::new(),
                flashblocks: HashMap::new(),
                flashblock_eviction_queue: VecDeque::new(),
                unreported_confirmations: VecDeque::new(),
                in_flight_per_sender,
                total_in_flight: 0,
                landed_blocks: BTreeSet::new(),
            })),
        }
    }

    /// Records transactions accepted by the submission RPC.
    pub fn sent_transactions(&self, transactions: Vec<SentTransaction>) {
        let submit_time = Instant::now();
        let mut inner = self.inner.write();

        for transaction in transactions {
            if inner.pending.contains_key(&transaction.tx_hash) {
                continue;
            }

            inner.pending.insert(
                transaction.tx_hash,
                PendingTransaction {
                    from: transaction.from,
                    submit_time,
                    in_flight_released: false,
                },
            );
            inner
                .in_flight_per_sender
                .entry(transaction.from)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
            inner.total_in_flight = inner.total_in_flight.saturating_add(1);
        }
    }

    /// Records transaction inclusions observed from the flashblock stream.
    ///
    /// When a pending transaction is seen in a flashblock, its in-flight slot is released
    /// immediately so the sender can submit new transactions without waiting for the slower
    /// canonical block receipt.
    pub fn on_new_flashblock(&self, inclusions: Vec<FlashblockInclusion>) {
        let mut inner = self.inner.write();

        for inclusion in inclusions {
            if let Entry::Vacant(e) = inner.flashblocks.entry(inclusion.tx_hash) {
                e.insert(inclusion.included_at);
                inner.flashblock_eviction_queue.push_back(inclusion.tx_hash);
            }

            if let Some(pending) = inner.pending.get_mut(&inclusion.tx_hash)
                && !pending.in_flight_released
            {
                pending.in_flight_released = true;
                let from = pending.from;
                inner.decrement_in_flight(&from);
            }
        }

        inner.evict_flashblocks();
    }

    /// Records the transaction hashes observed in a newly polled canonical block.
    ///
    /// This is the in-run landing detector: the first time one of our pending
    /// transactions is seen in a block's transaction list, its landing latency
    /// (submit -> first-seen, which includes the block poll + scan cost) is recorded,
    /// its block number is captured, and a [`TransactionMetrics`] entry is emitted.
    /// Gas, effective gas price, and revert status are left at defaults here and
    /// backfilled later by the end-of-run receipt pass.
    pub fn on_new_block_hashes(&self, block: BlockObservation, tx_hashes: Vec<TxHash>) {
        let mut inner = self.inner.write();

        for tx_hash in tx_hashes {
            inner.land_if_pending(tx_hash, &block);
        }
    }

    /// Expires submitted transactions that were not observed in a canonical block.
    ///
    /// Removes all pending entries older than `max_age`, regardless of whether their
    /// in-flight slot was already released. Returns the number of entries that were
    /// NOT previously confirmed by a flashblock (true failures).
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
            if let Some(pending) = inner.pending.remove(&tx_hash)
                && !pending.in_flight_released
            {
                inner.decrement_in_flight(&pending.from);
                unconfirmed_count += 1;
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
}

impl ResultsTrackerInner {
    /// Records the first observation of `tx_hash` in a polled block, emitting its
    /// landing metrics. Idempotent: a tx is removed from `pending` on first landing,
    /// so later blocks containing the same hash are ignored.
    fn land_if_pending(&mut self, tx_hash: TxHash, block: &BlockObservation) {
        let Some(pending) = self.pending.remove(&tx_hash) else {
            return;
        };

        let block_latency = block.observed_at.checked_duration_since(pending.submit_time);
        let flashblocks_latency = self
            .flashblocks
            .remove(&tx_hash)
            .and_then(|included_at| included_at.checked_duration_since(pending.submit_time));

        let mut metrics = TransactionMetrics::new(
            tx_hash,
            block_latency,
            flashblocks_latency,
            0,
            0,
            Some(block.number),
        );
        metrics.confirmed_at = Some(block.observed_at);

        self.landed_blocks.insert(block.number);
        if !pending.in_flight_released {
            self.decrement_in_flight(&pending.from);
        }
        self.unreported_confirmations.push_back(metrics);
    }

    fn decrement_in_flight(&mut self, from: &Address) {
        if let Some(count) = self.in_flight_per_sender.get_mut(from) {
            *count = count.saturating_sub(1);
        }
        self.total_in_flight = self.total_in_flight.saturating_sub(1);
    }

    fn evict_flashblocks(&mut self) {
        while self.flashblocks.len() > MAX_FLASHBLOCK_CACHE_SIZE {
            match self.flashblock_eviction_queue.pop_front() {
                Some(old) => {
                    self.flashblocks.remove(&old);
                }
                None => break,
            }
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

    #[test]
    fn confirms_pending_transaction_from_block_hashes() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(1);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
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
    }

    #[test]
    fn second_block_with_same_hash_is_ignored() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(7);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        let now = Instant::now();
        tracker.on_new_block_hashes(block_at(11, now + Duration::from_millis(100)), vec![tx_hash]);
        tracker.on_new_block_hashes(block_at(12, now + Duration::from_millis(300)), vec![tx_hash]);

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1, "tx should land exactly once despite reappearing");
        assert_eq!(metrics[0].block_number, Some(11), "first-seen block wins");
        assert_eq!(tracker.landed_block_numbers(), vec![11], "only first block tracked");
    }

    #[test]
    fn joins_flashblock_latency() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(2);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        let now = Instant::now();
        tracker.on_new_flashblock(vec![FlashblockInclusion {
            tx_hash,
            included_at: now + Duration::from_millis(50),
        }]);
        tracker.on_new_block_hashes(block_at(8, now + Duration::from_millis(200)), vec![tx_hash]);

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].flashblocks_latency.is_some(), "FB latency joined at landing");
    }

    #[test]
    fn flashblock_releases_in_flight_before_block_landing() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(4);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        assert_eq!(tracker.total_in_flight(), 1);
        assert_eq!(tracker.in_flight_for(&from), 1);

        tracker
            .on_new_flashblock(vec![FlashblockInclusion { tx_hash, included_at: Instant::now() }]);
        assert_eq!(tracker.total_in_flight(), 0, "flashblock should release in-flight slot");
        assert_eq!(tracker.in_flight_for(&from), 0);

        let observed_at = Instant::now() + Duration::from_millis(500);
        tracker.on_new_block_hashes(block_at(10, observed_at), vec![tx_hash]);

        assert_eq!(tracker.total_in_flight(), 0, "block landing should not double-decrement");
        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1, "metrics should still be produced from block landing");
        assert!(metrics[0].flashblocks_latency.is_some());
    }

    #[test]
    fn duplicate_flashblock_does_not_double_release() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(5);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        assert_eq!(tracker.total_in_flight(), 1);

        tracker
            .on_new_flashblock(vec![FlashblockInclusion { tx_hash, included_at: Instant::now() }]);
        assert_eq!(tracker.total_in_flight(), 0);

        // Duplicate flashblock event for same tx.
        tracker
            .on_new_flashblock(vec![FlashblockInclusion { tx_hash, included_at: Instant::now() }]);
        assert_eq!(tracker.total_in_flight(), 0, "duplicate flashblock should not underflow");
    }

    #[test]
    fn expire_pending_cleans_up_flashblock_released_entries() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(0xe0);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        assert_eq!(tracker.pending_count(), 1);

        // Flashblock confirms it — releases in-flight but keeps the pending entry.
        tracker
            .on_new_flashblock(vec![FlashblockInclusion { tx_hash, included_at: Instant::now() }]);
        assert_eq!(tracker.pending_count(), 1, "pending entry should still exist");
        assert_eq!(tracker.total_in_flight(), 0);

        // expire_pending should remove flashblock-released entries too.
        let expired = tracker.expire_pending(Duration::ZERO);
        assert_eq!(expired, 0, "flashblock-released tx is not a true failure");
        assert_eq!(tracker.pending_count(), 0, "pending entry should be cleaned up");
    }
}
