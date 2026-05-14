//! Result tracking for submitted transactions and inclusion observations.

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash};
use parking_lot::RwLock;

use crate::metrics::TransactionMetrics;

/// Maximum canonical receipt entries retained from recent blocks.
const MAX_RECEIPT_CACHE_SIZE: usize = 100_000;
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
    /// Local instant corresponding to the block timestamp, when available.
    pub block_time: Option<Instant>,
    /// Local time when the load-test process observed the block.
    pub observed_at: Instant,
}

/// Canonical receipt data observed for a transaction in a block.
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
    block_receipts: HashMap<TxHash, BlockReceiptInclusion>,
    flashblocks: HashMap<TxHash, Instant>,
    receipt_eviction_queue: VecDeque<TxHash>,
    flashblock_eviction_queue: VecDeque<TxHash>,
    unreported_confirmations: VecDeque<TransactionMetrics>,
    in_flight_per_sender: HashMap<Address, u64>,
    total_in_flight: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingTransaction {
    from: Address,
    submit_time: Instant,
}

#[derive(Debug, Clone, Copy)]
struct BlockReceiptInclusion {
    observed_at: Instant,
    block_time: Option<Instant>,
    block_number: u64,
    gas_used: u64,
    effective_gas_price: u128,
    success: bool,
}

impl ResultsTracker {
    /// Creates a new tracker for the given sender addresses.
    pub fn new(sender_addresses: &[Address]) -> Self {
        let in_flight_per_sender =
            sender_addresses.iter().copied().map(|address| (address, 0)).collect();
        Self {
            inner: Arc::new(RwLock::new(ResultsTrackerInner {
                pending: HashMap::new(),
                block_receipts: HashMap::new(),
                flashblocks: HashMap::new(),
                receipt_eviction_queue: VecDeque::new(),
                flashblock_eviction_queue: VecDeque::new(),
                unreported_confirmations: VecDeque::new(),
                in_flight_per_sender,
                total_in_flight: 0,
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
                PendingTransaction { from: transaction.from, submit_time },
            );
            inner
                .in_flight_per_sender
                .entry(transaction.from)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
            inner.total_in_flight = inner.total_in_flight.saturating_add(1);
            inner.confirm_if_ready(transaction.tx_hash);
        }
    }

    /// Records transaction inclusions observed from the flashblock stream.
    pub fn on_new_flashblock(&self, inclusions: Vec<FlashblockInclusion>) {
        let mut inner = self.inner.write();

        for inclusion in inclusions {
            if let Entry::Vacant(e) = inner.flashblocks.entry(inclusion.tx_hash) {
                e.insert(inclusion.included_at);
                inner.flashblock_eviction_queue.push_back(inclusion.tx_hash);
            }
        }

        inner.evict_flashblocks();
    }

    /// Records a newly observed canonical block and its receipts.
    pub fn on_new_block(&self, block: BlockObservation, receipts: Vec<BlockReceipt>) {
        let mut inner = self.inner.write();

        for receipt in receipts {
            let inclusion = BlockReceiptInclusion {
                observed_at: block.observed_at,
                block_time: block.block_time,
                block_number: receipt.block_number,
                gas_used: receipt.gas_used,
                effective_gas_price: receipt.effective_gas_price,
                success: receipt.success,
            };

            if let Entry::Vacant(e) = inner.block_receipts.entry(receipt.tx_hash) {
                e.insert(inclusion);
                inner.receipt_eviction_queue.push_back(receipt.tx_hash);
            }
            inner.confirm_if_ready(receipt.tx_hash);
        }

        inner.evict_block_receipts();
    }

    /// Expires submitted transactions that were not observed in a canonical block.
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

        let expired_count = expired.len() as u64;
        for tx_hash in expired {
            if let Some(pending) = inner.pending.remove(&tx_hash) {
                inner.decrement_in_flight(&pending.from);
            }
        }

        expired_count
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
}

impl ResultsTrackerInner {
    fn confirm_if_ready(&mut self, tx_hash: TxHash) {
        let Some(receipt) = self.block_receipts.get(&tx_hash).copied() else {
            return;
        };
        let Some(pending) = self.pending.remove(&tx_hash) else {
            return;
        };

        let block_latency = receipt
            .block_time
            .map(|block_time| block_time.saturating_duration_since(pending.submit_time));
        let block_receipt_delay = receipt
            .block_time
            .map(|block_time| receipt.observed_at.saturating_duration_since(block_time));
        let flashblocks_latency = self
            .flashblocks
            .remove(&tx_hash)
            .and_then(|included_at| included_at.checked_duration_since(pending.submit_time));

        let mut metrics = TransactionMetrics::new(
            tx_hash,
            block_latency,
            flashblocks_latency,
            receipt.gas_used,
            receipt.effective_gas_price,
            Some(receipt.block_number),
        );
        metrics.block_receipt_delay = block_receipt_delay;
        metrics.confirmed_at = Some(receipt.observed_at);
        metrics.reverted = !receipt.success;

        self.block_receipts.remove(&tx_hash);
        self.decrement_in_flight(&pending.from);
        self.unreported_confirmations.push_back(metrics);
    }

    fn decrement_in_flight(&mut self, from: &Address) {
        if let Some(count) = self.in_flight_per_sender.get_mut(from) {
            *count = count.saturating_sub(1);
        }
        self.total_in_flight = self.total_in_flight.saturating_sub(1);
    }

    fn evict_block_receipts(&mut self) {
        while self.block_receipts.len() > MAX_RECEIPT_CACHE_SIZE {
            match self.receipt_eviction_queue.pop_front() {
                Some(old) => {
                    self.block_receipts.remove(&old);
                }
                None => break,
            }
        }
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

    #[test]
    fn confirms_pending_transaction_from_block_receipt() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(1);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        let block_time = Instant::now() + Duration::from_millis(50);
        tracker.on_new_block(
            BlockObservation {
                number: 7,
                block_time: Some(block_time),
                observed_at: block_time + Duration::from_millis(250),
            },
            vec![BlockReceipt {
                tx_hash,
                block_number: 7,
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                success: true,
            }],
        );

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].tx_hash, tx_hash);
        assert_eq!(metrics[0].block_number, Some(7));
        assert_eq!(metrics[0].gas_used, 21_000);
        assert!(!metrics[0].reverted);
        assert_eq!(metrics[0].block_receipt_delay, Some(Duration::from_millis(250)));
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn marks_reverted_transaction() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(3);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        let block_time = Instant::now() + Duration::from_millis(50);
        tracker.on_new_block(
            BlockObservation {
                number: 9,
                block_time: Some(block_time),
                observed_at: block_time + Duration::from_millis(100),
            },
            vec![BlockReceipt {
                tx_hash,
                block_number: 9,
                gas_used: 45_000,
                effective_gas_price: 1_000_000_000,
                success: false,
            }],
        );

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1, "reverted tx should still produce metrics");
        assert_eq!(metrics[0].tx_hash, tx_hash);
        assert!(metrics[0].reverted, "reverted flag should be set");
        assert_eq!(metrics[0].gas_used, 45_000);
        assert_eq!(tracker.total_in_flight(), 0);
    }

    #[test]
    fn joins_flashblock_latency() {
        let from = address!("0000000000000000000000000000000000000001");
        let tx_hash = TxHash::repeat_byte(2);
        let tracker = ResultsTracker::new(&[from]);

        tracker.sent_transactions(vec![SentTransaction { tx_hash, from }]);
        tracker
            .on_new_flashblock(vec![FlashblockInclusion { tx_hash, included_at: Instant::now() }]);
        tracker.on_new_block(
            BlockObservation {
                number: 8,
                block_time: Some(Instant::now()),
                observed_at: Instant::now(),
            },
            vec![BlockReceipt {
                tx_hash,
                block_number: 8,
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                success: true,
            }],
        );

        let metrics = tracker.drain_confirmed_metrics();
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].flashblocks_latency.is_some());
    }
}
