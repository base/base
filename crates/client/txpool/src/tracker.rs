//! Transaction tracking state machine powering the tracex execution extension.

use std::{
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use base_flashblocks::PendingBlocks;
use chrono::Local;
use lru::LruCache;
use reth_node_api::{BlockBody, NodePrimitives};
use reth_primitives_traits::transaction::TxHashRef;
use reth_provider::{CanonStateNotification, Chain};
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{FullTransactionEvent, PoolTransaction};

use crate::{EventLog, Metrics, Pool, TxEvent};

/// Tracks transactions as they move through the mempool and into blocks.
#[derive(Debug, Clone)]
pub struct Tracker {
    /// Map of transaction hash to timestamp when first seen in mempool.
    txs: LruCache<TxHash, EventLog>,
    /// Map of transaction hash to current state.
    tx_states: LruCache<TxHash, Pool>,
    /// Enable `info` logs for transaction tracing.
    enable_logs: bool,
    /// Metrics for the `reth_transaction_tracing` component.
    metrics: Metrics,
}

impl Tracker {
    /// Max size of the LRU caches.
    pub const MAX_SIZE: usize = 20_000;

    /// Create a new tracker.
    pub fn new(enable_logs: bool) -> Self {
        Self {
            txs: LruCache::new(NonZeroUsize::new(Self::MAX_SIZE).expect("non zero")),
            tx_states: LruCache::new(NonZeroUsize::new(Self::MAX_SIZE).expect("non zero")),
            enable_logs,
            metrics: Metrics::default(),
        }
    }

    /// Parse [`FullTransactionEvent`]s and update the tracker.
    pub fn handle_event<T: PoolTransaction>(&mut self, event: FullTransactionEvent<T>) {
        match event {
            FullTransactionEvent::Pending(tx_hash) => {
                self.transaction_inserted(tx_hash, TxEvent::Pending);
                self.transaction_moved(tx_hash, Pool::Pending);
            }
            FullTransactionEvent::Queued(tx_hash, _) => {
                self.transaction_inserted(tx_hash, TxEvent::Queued);
                self.transaction_moved(tx_hash, Pool::Queued);
            }
            FullTransactionEvent::Discarded(tx_hash) => {
                self.transaction_completed(tx_hash, TxEvent::Dropped);
            }
            FullTransactionEvent::Replaced { transaction, replaced_by } => {
                let tx_hash = transaction.hash();
                self.transaction_replaced(*tx_hash, TxHash::from(replaced_by));
            }
            _ => {
                // Other events.
            }
        }
    }

    /// Parse [`CanonStateNotification`]s and update the tracker.
    pub fn handle_canon_state_notification<N: NodePrimitives>(
        &mut self,
        notification: CanonStateNotification<N>,
    ) {
        self.track_committed_chain(&notification.committed());
    }

    /// Parse flashblock updates and track transaction inclusion in flashblocks.
    pub fn handle_flashblock_notification(&mut self, pending_blocks: Arc<PendingBlocks>) {
        self.track_flashblock_transactions(&pending_blocks);
    }

    fn track_committed_chain<N: NodePrimitives>(&mut self, chain: &Chain<N>) {
        for block in chain.blocks().values() {
            for transaction in block.body().transactions() {
                self.transaction_completed(*transaction.tx_hash(), TxEvent::BlockInclusion);
            }
        }
    }

    fn track_flashblock_transactions(&mut self, pending_blocks: &PendingBlocks) {
        // Get all transaction hashes from pending blocks
        for tx_hash in pending_blocks.get_pending_transaction_hashes() {
            self.transaction_fb_included(tx_hash);
        }
    }

    /// Track the first time we see a transaction in the mempool.
    pub fn transaction_inserted(&mut self, tx_hash: TxHash, event: TxEvent) {
        // If we've seen the tx before, don't track it again. For example,
        // if a tx was pending then moved to queued, we don't want to update the timestamp
        // with the queued timestamp.
        if self.txs.contains(&tx_hash) {
            return;
        }

        // If the LRU is full and we're about to insert a new tx, log the `EventLog` for that tx
        // before it gets evicted. This can be useful to see the full history of a transaction.
        if self.txs.len() == Self::MAX_SIZE
            && let Some((tx_hash, event_log)) = self.txs.peek_lru()
        {
            self.log(tx_hash, event_log, "Transaction inserted");
        }

        self.txs.put(tx_hash, EventLog::new(Local::now(), event));
    }

    /// Track a transaction moving from one pool to another.
    pub fn transaction_moved(&mut self, tx_hash: TxHash, pool: Pool) {
        // If we've seen the transaction pending or queued before, track the pending <> queue transition.
        if let Some(prev_pool) = self.tx_states.get(&tx_hash)
            && prev_pool != &pool
        {
            let event = match (prev_pool, &pool) {
                (Pool::Pending, Pool::Queued) => Some(TxEvent::PendingToQueued),
                (Pool::Queued, Pool::Pending) => Some(TxEvent::QueuedToPending),
                _ => None,
            };

            if let (Some(event), Some(mut event_log)) = (event, self.txs.pop(&tx_hash)) {
                let mempool_time = event_log.mempool_time;
                let time_in_mempool = Instant::now().duration_since(mempool_time);

                if self.is_overflowed(&tx_hash, &event_log) {
                    // The tx is already removed from the cache from `pop`.
                    return;
                }

                // Set pending_time if transitioning to pending
                if event == TxEvent::QueuedToPending && event_log.pending_time.is_none() {
                    event_log.pending_time = Some(Instant::now());
                }

                event_log.push(Local::now(), event);
                self.txs.put(tx_hash, event_log);

                Self::record_histogram(time_in_mempool, event);
            }
        }

        // Update the new pool the transaction is in.
        self.tx_states.put(tx_hash, pool.clone());
        debug!(target: "tracex", tx_hash = ?tx_hash, state = ?pool, "Transaction moved pools");
    }

    /// Track a transaction being included in a block or dropped.
    pub fn transaction_completed(&mut self, tx_hash: TxHash, event: TxEvent) {
        if let Some(mut event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = Instant::now().duration_since(mempool_time);

            if self.is_overflowed(&tx_hash, &event_log) {
                return;
            }
            // Don't add it back to LRU so that we keep the LRU cache size small which will help longer-lived txs
            // but do update the event log with the final event (i.e., included/dropped).
            event_log.push(Local::now(), event);

            // Record `inclusion_duration` metric if transaction was pending and is now included
            if event == TxEvent::BlockInclusion
                && let Some(pending_time) = event_log.pending_time
            {
                let time_pending_to_inclusion = Instant::now().duration_since(pending_time);
                self.metrics
                    .inclusion_duration
                    .record(time_pending_to_inclusion.as_millis() as f64);
            }

            // If a tx is included/dropped, log it now.
            self.log(&tx_hash, &event_log, &format!("Transaction {event}"));
            Self::record_histogram(time_in_mempool, event);
        }
    }

    /// Track a transaction being included in a flashblock. This will not remove
    /// the tx from the cache.
    pub fn transaction_fb_included(&mut self, tx_hash: TxHash) {
        // Only track if we have seen this transaction before
        if let Some(event_log) = self.txs.peek(&tx_hash) {
            // Record `fb_inclusion_duration` metric if transaction was pending
            if let Some(pending_time) = event_log.pending_time {
                let time_pending_to_fb_inclusion = Instant::now().duration_since(pending_time);
                self.metrics
                    .fb_inclusion_duration
                    .record(time_pending_to_fb_inclusion.as_millis() as f64);

                debug!(
                    target: "tracex",
                    tx_hash = ?tx_hash,
                    duration_ms = time_pending_to_fb_inclusion.as_millis(),
                    "Transaction included in flashblock"
                );
            }
        }
    }

    /// Track a transaction being replaced by removing it from the cache and adding the new tx.
    pub fn transaction_replaced(&mut self, tx_hash: TxHash, replaced_by: TxHash) {
        if let Some(mut event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = Instant::now().duration_since(mempool_time);
            debug!(target: "tracex", tx_hash = ?tx_hash, replaced_by = ?replaced_by, "Transaction replaced");

            if self.is_overflowed(&tx_hash, &event_log) {
                return;
            }
            // Keep the event log and update the tx hash.
            event_log.push(Local::now(), TxEvent::Replaced);
            self.txs.put(replaced_by, event_log);

            Self::record_histogram(time_in_mempool, TxEvent::Replaced);
        }
    }

    /// Logs an [`EventLog`] through tracing.
    fn log(&self, tx_hash: &TxHash, event_log: &EventLog, msg: &str) {
        if !self.enable_logs {
            return;
        }

        let events = event_log.to_vec();
        if !events.is_empty() {
            info!(target: "tracex", tx_hash = ?tx_hash, events = ?events, %msg);
        }
    }

    // If `is_overflowed` is true then we record an overflowed metric and log the event log
    // and don't record the other event that was supposed to be recorded.
    fn is_overflowed(&self, tx_hash: &TxHash, event_log: &EventLog) -> bool {
        if event_log.events.len() < event_log.limit {
            return false;
        }

        self.log(tx_hash, event_log, "Transaction removed from cache due to limit");
        Self::record_histogram(event_log.mempool_time.elapsed(), TxEvent::Overflowed);
        true
    }

    /// Records a metrics histogram. We have to use `histogram!` here because it supports tags.
    fn record_histogram(time_in_mempool: Duration, event: TxEvent) {
        metrics::histogram!("reth_transaction_tracing_tx_event", "event" => event.to_string())
            .record(time_in_mempool.as_millis() as f64);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Deref;

    use alloy_primitives::{Address, B256, Bytes, U256};
    use base_client_node::test_utils::{Account, L1_BLOCK_INFO_DEPOSIT_TX};
    use base_flashblocks::FlashblocksAPI;
    use base_flashblocks_node::test_harness::FlashblocksHarness;
    use base_flashtypes::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_primitives::build_eip1559_tx;
    use tokio::time;

    use super::*;

    /// Helper to build a test flashblock with transactions
    fn build_test_flashblock(block_number: u64, transactions: Vec<Bytes>) -> Flashblock {
        Flashblock {
            payload_id: alloy_rpc_types_engine::PayloadId::new([0; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::default(),
                parent_hash: B256::default(),
                fee_recipient: Address::ZERO,
                prev_randao: B256::default(),
                block_number,
                gas_limit: 30_000_000,
                timestamp: 0,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::ZERO,
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                blob_gas_used: Some(0),
                transactions: [vec![L1_BLOCK_INFO_DEPOSIT_TX], transactions].concat(),
                ..Default::default()
            },
            metadata: Metadata { block_number },
        }
    }

    #[test]
    fn test_transaction_inserted_pending() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Insert a pending transaction
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        assert_eq!(tracker.txs.len(), 1);

        let event_log = tracker.txs.get(&tx_hash).expect("tx should exist");
        assert_eq!(event_log.events.len(), 1);
        assert_eq!(event_log.events[0].1, TxEvent::Pending);
        // Pending transactions should have pending_time set
        assert!(event_log.pending_time.is_some());
    }

    #[test]
    fn test_transaction_inserted_queued() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Insert a queued transaction
        tracker.transaction_inserted(tx_hash, TxEvent::Queued);
        assert_eq!(tracker.txs.len(), 1);

        let event_log = tracker.txs.get(&tx_hash).expect("tx should exist");
        assert_eq!(event_log.events.len(), 1);
        assert_eq!(event_log.events[0].1, TxEvent::Queued);
        // Queued transactions should not have pending_time set yet
        assert!(event_log.pending_time.is_none());
    }

    #[test]
    fn test_transaction_inserted_duplicate_ignored() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Insert same transaction twice
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        let first_mempool_time = tracker.txs.get(&tx_hash).unwrap().mempool_time;

        // Second insert should be ignored
        tracker.transaction_inserted(tx_hash, TxEvent::Queued);
        assert_eq!(tracker.txs.len(), 1);

        let event_log = tracker.txs.get(&tx_hash).unwrap();
        // Should still have only 1 event (the first one)
        assert_eq!(event_log.events.len(), 1);
        assert_eq!(event_log.events[0].1, TxEvent::Pending);
        // mempool_time should not have changed
        assert_eq!(event_log.mempool_time, first_mempool_time);
    }

    #[test]
    fn test_transaction_moved_queued_to_pending() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Start with queued transaction
        tracker.transaction_inserted(tx_hash, TxEvent::Queued);
        tracker.transaction_moved(tx_hash, Pool::Queued);

        // Verify no pending_time initially
        assert!(tracker.txs.get(&tx_hash).unwrap().pending_time.is_none());

        // Move to pending
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Verify event was logged and pending_time was set
        let event_log = tracker.txs.get(&tx_hash).expect("tx should exist");
        assert_eq!(event_log.events.len(), 2);
        assert_eq!(event_log.events[1].1, TxEvent::QueuedToPending);
        assert!(event_log.pending_time.is_some());
    }

    #[test]
    fn test_transaction_moved_pending_to_queued() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Start with pending transaction
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Verify pending_time is set
        assert!(tracker.txs.get(&tx_hash).unwrap().pending_time.is_some());
        let pending_time = tracker.txs.get(&tx_hash).unwrap().pending_time;

        // Move to queued
        tracker.transaction_moved(tx_hash, Pool::Queued);

        // Verify event was logged and pending_time is preserved
        let event_log = tracker.txs.get(&tx_hash).expect("tx should exist");
        assert_eq!(event_log.events.len(), 2);
        assert_eq!(event_log.events[1].1, TxEvent::PendingToQueued);
        // pending_time should be preserved (not reset)
        assert_eq!(event_log.pending_time, pending_time);
    }

    #[test]
    fn test_transaction_moved_same_pool_no_event() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Insert and move to pending
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Try moving to same pool again
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Should still only have 1 event
        let event_log = tracker.txs.get(&tx_hash).expect("tx should exist");
        assert_eq!(event_log.events.len(), 1);
    }

    #[test]
    fn test_transaction_completed_block_inclusion_with_pending_time() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Create a pending transaction (which sets pending_time)
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Verify pending_time is set
        assert!(tracker.txs.peek(&tx_hash).unwrap().pending_time.is_some());

        // Complete the transaction with block inclusion
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion);

        // Transaction should be removed from txs cache
        assert!(tracker.txs.get(&tx_hash).is_none());
    }

    #[test]
    fn test_transaction_completed_dropped() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Insert transaction
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);

        // Drop the transaction
        tracker.transaction_completed(tx_hash, TxEvent::Dropped);

        // Transaction should be removed from cache
        assert!(tracker.txs.get(&tx_hash).is_none());
    }

    #[test]
    fn test_transaction_replaced() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();
        let replacement_hash = TxHash::random();

        // Insert original transaction
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        assert_eq!(tracker.txs.len(), 1);

        // Replace transaction
        tracker.transaction_replaced(tx_hash, replacement_hash);

        // Original should be gone, replacement should exist
        assert!(tracker.txs.get(&tx_hash).is_none());
        assert!(tracker.txs.get(&replacement_hash).is_some());

        // Event log should be preserved with replacement event
        let event_log = tracker.txs.get(&replacement_hash).unwrap();
        assert_eq!(event_log.events.len(), 2);
        assert_eq!(event_log.events[0].1, TxEvent::Pending);
        assert_eq!(event_log.events[1].1, TxEvent::Replaced);
    }

    #[test]
    fn test_transaction_replaced_nonexistent() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();
        let replacement_hash = TxHash::random();

        // Try to replace a transaction that doesn't exist
        tracker.transaction_replaced(tx_hash, replacement_hash);

        // Nothing should happen
        assert_eq!(tracker.txs.len(), 0);
    }

    #[test]
    fn test_is_overflowed() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Create an event log - starts with 1 event (Pending)
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Add events until we hit the limit (limit is 10, we have 1 event already)
        for _ in 0..9 {
            if let Some(mut event_log) = tracker.txs.pop(&tx_hash) {
                event_log.push(Local::now(), TxEvent::PendingToQueued);
                tracker.txs.put(tx_hash, event_log);
            }
        }

        // Verify we're at the limit
        assert_eq!(tracker.txs.get(&tx_hash).unwrap().events.len(), 10);

        // Try to move again - should trigger overflow check
        tracker.transaction_moved(tx_hash, Pool::Queued);

        // Transaction should be removed due to overflow
        assert!(tracker.txs.get(&tx_hash).is_none());
    }

    #[test]
    fn test_pending_time_set_only_once() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // Start with queued
        tracker.transaction_inserted(tx_hash, TxEvent::Queued);
        tracker.transaction_moved(tx_hash, Pool::Queued);
        assert!(tracker.txs.get(&tx_hash).unwrap().pending_time.is_none());

        // Move to pending (should set pending_time)
        tracker.transaction_moved(tx_hash, Pool::Pending);
        let first_pending_time = tracker.txs.get(&tx_hash).unwrap().pending_time;
        assert!(first_pending_time.is_some());

        // Move back to queued
        tracker.transaction_moved(tx_hash, Pool::Queued);

        // Move to pending again (should NOT reset pending_time)
        tracker.transaction_moved(tx_hash, Pool::Pending);
        let second_pending_time = tracker.txs.get(&tx_hash).unwrap().pending_time;

        // pending_time should be the same as the first time
        assert_eq!(first_pending_time, second_pending_time);
    }

    #[test]
    fn test_full_transaction_lifecycle_queued_to_pending_to_inclusion() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // 1. Transaction enters as queued
        tracker.transaction_inserted(tx_hash, TxEvent::Queued);
        tracker.transaction_moved(tx_hash, Pool::Queued);
        assert_eq!(tracker.txs.len(), 1);
        assert!(tracker.txs.get(&tx_hash).unwrap().pending_time.is_none());

        // 2. Transaction moves to pending
        tracker.transaction_moved(tx_hash, Pool::Pending);
        assert!(tracker.txs.get(&tx_hash).unwrap().pending_time.is_some());

        let event_log = tracker.txs.get(&tx_hash).unwrap();
        assert_eq!(event_log.events.len(), 2);
        assert_eq!(event_log.events[0].1, TxEvent::Queued);
        assert_eq!(event_log.events[1].1, TxEvent::QueuedToPending);

        // 3. Transaction included in block
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion);
        assert!(tracker.txs.get(&tx_hash).is_none());
    }

    #[test]
    fn test_full_transaction_lifecycle_pending_to_inclusion() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        // 1. Transaction enters as pending
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.transaction_moved(tx_hash, Pool::Pending);
        assert_eq!(tracker.txs.len(), 1);
        assert!(tracker.txs.get(&tx_hash).unwrap().pending_time.is_some());

        // 2. Transaction included in block
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion);
        assert!(tracker.txs.get(&tx_hash).is_none());
    }

    #[test]
    fn test_multiple_transactions_independence() {
        let mut tracker = Tracker::new(false);
        let tx_hash1 = TxHash::random();
        let tx_hash2 = TxHash::random();

        // Insert two different transactions
        tracker.transaction_inserted(tx_hash1, TxEvent::Pending);
        tracker.transaction_inserted(tx_hash2, TxEvent::Queued);

        assert_eq!(tracker.txs.len(), 2);

        // Verify they're tracked independently
        assert!(tracker.txs.get(&tx_hash1).unwrap().pending_time.is_some());
        assert!(tracker.txs.get(&tx_hash2).unwrap().pending_time.is_none());

        // Complete one
        tracker.transaction_completed(tx_hash1, TxEvent::BlockInclusion);

        // Only one should remain
        assert_eq!(tracker.txs.len(), 1);
        assert!(tracker.txs.get(&tx_hash2).is_some());
    }

    #[tokio::test]
    async fn test_fb_inclusion() -> eyre::Result<()> {
        // Setup
        let harness = FlashblocksHarness::new().await?;
        let mut tracker = Tracker::new(false);

        // Build transaction
        let tx = build_eip1559_tx(
            harness.chain_id(),
            0,
            Account::Alice.address(),
            U256::ZERO,
            Bytes::new(),
            Account::Alice,
        );
        let tx_hash = alloy_primitives::keccak256(&tx);
        let fb = build_test_flashblock(1, vec![tx]);

        // Mimic sending a tx to the mpool/builder
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);

        // Wait a bit to simulate builder picking and building the tx into the pending block
        time::sleep(Duration::from_millis(10)).await;
        harness.send_flashblock(fb).await?;

        let state = harness.flashblocks_state().get_pending_blocks();
        // Verify we have some pending transactions
        let ptxs = state.as_ref().map(|pb| pb.get_pending_transaction_hashes()).unwrap_or_default();
        assert_eq!(ptxs.len(), 2); // L1Info + tx
        assert_eq!(ptxs[1], tx_hash);

        let pb = state.as_ref().unwrap().deref();
        tracker.track_flashblock_transactions(pb);

        // It should still be in the tracker
        assert!(tracker.txs.get(&tx_hash).is_some());

        // Wait until its included in canonical block
        time::sleep(Duration::from_millis(1500)).await;
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion);

        // It should be removed from the tracker
        assert!(tracker.txs.get(&tx_hash).is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_can_receive_fb() -> eyre::Result<()> {
        // Setup
        let harness = FlashblocksHarness::new().await?;
        let mut tracker = Tracker::new(false);

        // Subscribe to flashblocks
        let mut stream = harness.flashblocks_state().subscribe_to_flashblocks();
        let mut t = tracker.clone();

        // Use a oneshot channel to signal when we receive a flashblock
        let (tx_signal, rx_signal) = tokio::sync::oneshot::channel();
        let mut tx_signal = Some(tx_signal);

        tokio::spawn(async move {
            while let Ok(pending_blocks) = stream.recv().await {
                t.handle_flashblock_notification(pending_blocks);
                // Signal that we received a flashblock
                if let Some(signal) = tx_signal.take() {
                    let _ = signal.send(());
                }
            }
        });

        // Create a tx and send
        let tx = build_eip1559_tx(
            harness.chain_id(),
            0,
            Account::Alice.address(),
            U256::ZERO,
            Bytes::new(),
            Account::Alice,
        );
        let tx_hash = alloy_primitives::keccak256(&tx);
        let fb = build_test_flashblock(1, vec![tx]);

        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        // Send the flashblock
        harness.send_flashblock(fb).await?;

        // Verify we received the flashblock by waiting for the signal
        tokio::time::timeout(std::time::Duration::from_secs(1), rx_signal)
            .await
            .expect("timeout waiting for flashblock")
            .expect("channel closed before receiving flashblock");

        Ok(())
    }
}
