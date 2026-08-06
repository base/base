//! Transaction tracking state machine powering the tracex execution extension.

use std::{
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use base_flashblocks::PendingBlocks;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use chrono::Local;
use lru::LruCache;
use reth_node_api::{BlockBody, NodePrimitives};
use reth_primitives_traits::transaction::TxHashRef;
use reth_provider::{CanonStateNotification, Chain};
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{FullTransactionEvent, PoolTransaction};
use serde_json::{Map, Value, json};

use crate::{EventLog, NonceSlot, NonceSummary, Pool, TxEvent, metrics::Metrics};

/// Tracks transactions as they move through the mempool and into blocks.
#[derive(Debug, Clone)]
pub struct Tracker {
    /// Map of transaction hash to timestamp when first seen in mempool.
    txs: LruCache<TxHash, EventLog>,
    /// Map of transaction hash to current state.
    tx_states: LruCache<TxHash, Pool>,
    /// Map of tx hash to its nonce slot for reverse lookup on inclusion.
    tx_nonce_slots: LruCache<TxHash, NonceSlot>,
    /// Tracks end-to-end lifecycle per `(sender, nonce)` across replacements.
    nonce_summaries: LruCache<NonceSlot, NonceSummary>,
    /// Enable `info` logs for transaction tracing.
    enable_logs: bool,
    /// Optional node role label included in journal event data.
    node_role: Option<String>,
}

impl Tracker {
    /// Max size of the LRU caches.
    pub const MAX_SIZE: usize = 20_000;

    /// Block inclusion duration above this threshold increments the slow counter.
    const SLOW_BLOCK_INCLUSION_THRESHOLD: Duration = Duration::from_secs(3);
    /// Flashblock inclusion duration above this threshold increments the slow counter.
    const SLOW_FLASHBLOCK_INCLUSION_THRESHOLD: Duration = Duration::from_millis(1000);
    /// Producer-local event source label.
    const EVENT_SOURCE: &'static str = "txpool-tracing";

    /// Create a new tracker.
    pub fn new(enable_logs: bool) -> Self {
        Self::new_with_node_role(enable_logs, None)
    }

    /// Create a new tracker with an optional node role label.
    pub fn new_with_node_role(enable_logs: bool, node_role: Option<String>) -> Self {
        let cache_size = NonZeroUsize::new(Self::MAX_SIZE).expect("non zero");
        Self {
            txs: LruCache::new(cache_size),
            tx_states: LruCache::new(cache_size),
            tx_nonce_slots: LruCache::new(cache_size),
            nonce_summaries: LruCache::new(cache_size),
            enable_logs,
            node_role,
        }
    }

    /// Parse [`FullTransactionEvent`]s and update the tracker.
    ///
    /// `nonce_slot` is populated by the subscription layer for events that only
    /// carry a [`TxHash`] (Pending, Queued) by looking up the pool.
    pub fn handle_event<T: PoolTransaction>(
        &mut self,
        event: FullTransactionEvent<T>,
        nonce_slot: Option<NonceSlot>,
    ) {
        match event {
            FullTransactionEvent::Pending(tx_hash) => {
                self.transaction_inserted(tx_hash, TxEvent::Pending);
                self.transaction_moved(tx_hash, Pool::Pending);
                if let Some(slot) = nonce_slot {
                    self.track_nonce_slot(tx_hash, slot);
                }
            }
            FullTransactionEvent::Queued(tx_hash, _) => {
                self.transaction_inserted(tx_hash, TxEvent::Queued);
                self.transaction_moved(tx_hash, Pool::Queued);
                if let Some(slot) = nonce_slot {
                    self.track_nonce_slot(tx_hash, slot);
                }
            }
            FullTransactionEvent::Discarded(tx_hash) => {
                self.transaction_completed(tx_hash, TxEvent::Dropped, Instant::now());
            }
            FullTransactionEvent::Replaced { transaction, replaced_by } => {
                let sender = transaction.sender();
                let nonce = transaction.nonce();
                let tx_hash = *transaction.hash();
                let replaced_by = TxHash::from(replaced_by);
                self.transaction_replaced(tx_hash, replaced_by);
                let slot = NonceSlot::new(sender, nonce);
                self.nonce_replacement(slot);
                self.track_nonce_slot(replaced_by, slot);
            }
            _ => {}
        }
    }

    /// Parse [`CanonStateNotification`]s and update the tracker.
    pub fn handle_canon_state_notification<N: NodePrimitives>(
        &mut self,
        notification: CanonStateNotification<N>,
        received_at: Instant,
    ) {
        self.track_committed_chain(&notification.committed(), received_at);
    }

    /// Parse flashblock updates and track transaction inclusion in flashblocks.
    pub fn handle_flashblock_notification(
        &mut self,
        pending_blocks: Arc<PendingBlocks>,
        received_at: Instant,
    ) {
        self.track_flashblock_transactions(&pending_blocks, received_at);
    }

    fn track_committed_chain<N: NodePrimitives>(&mut self, chain: &Chain<N>, received_at: Instant) {
        for block in chain.blocks().values() {
            for transaction in block.body().transactions() {
                self.transaction_completed(
                    *transaction.tx_hash(),
                    TxEvent::BlockInclusion,
                    received_at,
                );
            }
        }
    }

    fn track_flashblock_transactions(
        &mut self,
        pending_blocks: &PendingBlocks,
        received_at: Instant,
    ) {
        // Get all transaction hashes from pending blocks
        for tx_hash in pending_blocks.get_pending_transaction_hashes() {
            self.transaction_fb_included(tx_hash, received_at);
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
            self.emit_transaction_event(
                *tx_hash,
                TxEvent::Overflowed,
                event_log.events.len(),
                TxpoolEventData {
                    overflow_reason: Some("tracker_lru_eviction"),
                    time_in_mempool: Some(event_log.mempool_time.elapsed()),
                    ..Default::default()
                },
            );
        }

        self.txs.put(tx_hash, EventLog::new(Local::now(), event));
        self.emit_transaction_event(
            tx_hash,
            event,
            0,
            TxpoolEventData {
                pool: match event {
                    TxEvent::Pending => Some(Pool::Pending),
                    TxEvent::Queued => Some(Pool::Queued),
                    _ => None,
                },
                ..Default::default()
            },
        );
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

                // Reset pending_time when transitioning to pending so that
                // inclusion duration only measures time actually spent in the
                // pending subpool, not time spent in queued/basefee.
                if event == TxEvent::QueuedToPending {
                    event_log.pending_time = Some(Instant::now());
                    event_log.fb_included = false;
                }

                event_log.push(Local::now(), event);
                let event_index = event_log.events.len() - 1;
                self.txs.put(tx_hash, event_log);

                self.emit_transaction_event(
                    tx_hash,
                    event,
                    event_index,
                    TxpoolEventData {
                        pool: Some(pool.clone()),
                        time_in_mempool: Some(time_in_mempool),
                        ..Default::default()
                    },
                );
                Self::record_histogram(time_in_mempool, event);
            }
        }

        // Update the new pool the transaction is in.
        self.tx_states.put(tx_hash, pool.clone());
        debug!(target: "tracex", tx_hash = ?tx_hash, state = ?pool, "Transaction moved pools");
    }

    /// Track a transaction being included in a block or dropped.
    pub fn transaction_completed(&mut self, tx_hash: TxHash, event: TxEvent, received_at: Instant) {
        if let Some(mut event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = received_at.duration_since(mempool_time);

            if self.is_overflowed(&tx_hash, &event_log) {
                return;
            }
            // Don't add it back to LRU so that we keep the LRU cache size small which will help longer-lived txs
            // but do update the event log with the final event (i.e., included/dropped).
            event_log.push(Local::now(), event);

            let time_pending_to_inclusion = if event == TxEvent::BlockInclusion {
                event_log.pending_time.map(|pending_time| received_at.duration_since(pending_time))
            } else {
                None
            };

            if event == TxEvent::BlockInclusion
                && let Some(time_pending_to_inclusion) = time_pending_to_inclusion
            {
                Metrics::inclusion_duration().record(time_pending_to_inclusion.as_millis() as f64);

                if time_pending_to_inclusion > Self::SLOW_BLOCK_INCLUSION_THRESHOLD {
                    Metrics::slow_inclusions().increment(1);
                } else {
                    Metrics::healthy_inclusions().increment(1);
                }
            }

            self.nonce_completed(&tx_hash, &event, received_at);
            self.log(&tx_hash, &event_log, &format!("Transaction {event}"));
            // Inclusion is journaled by the builder; tracer emits cover mempool lifecycle only.
            if event != TxEvent::BlockInclusion {
                self.emit_transaction_event(
                    tx_hash,
                    event,
                    event_log.events.len() - 1,
                    TxpoolEventData {
                        time_in_mempool: Some(time_in_mempool),
                        ..Default::default()
                    },
                );
            }
            Self::record_histogram(time_in_mempool, event);
        }
    }

    /// Track a transaction being included in a flashblock. This will not remove
    /// the tx from the cache.
    ///
    /// The `fb_included` flag on [`EventLog`] ensures that the metric is only
    /// recorded once per transaction, even when [`PendingBlocks`] contains
    /// transactions from earlier flashblocks that have already been measured.
    ///
    /// Flashblock inclusion is not written to the transaction event journal here;
    /// builder flashblock/inclusion events cover that path.
    pub fn transaction_fb_included(&mut self, tx_hash: TxHash, received_at: Instant) {
        // Only track if we have seen this transaction before and it hasn't
        // already been recorded as included in a flashblock.
        if let Some(event_log) = self.txs.get_mut(&tx_hash) {
            if event_log.fb_included {
                return;
            }

            if let Some(pending_time) = event_log.pending_time {
                let time_pending_to_fb_inclusion = received_at.duration_since(pending_time);
                Metrics::fb_inclusion_duration()
                    .record(time_pending_to_fb_inclusion.as_millis() as f64);

                if time_pending_to_fb_inclusion > Self::SLOW_FLASHBLOCK_INCLUSION_THRESHOLD {
                    Metrics::fb_slow_inclusions().increment(1);
                } else {
                    Metrics::fb_healthy_inclusions().increment(1);
                }

                debug!(
                    target: "tracex",
                    tx_hash = ?tx_hash,
                    duration_ms = time_pending_to_fb_inclusion.as_millis(),
                    "Transaction included in flashblock"
                );
            }

            event_log.fb_included = true;
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
            event_log.push(Local::now(), TxEvent::Replaced);
            let event_index = event_log.events.len() - 1;
            // Reset pending_time so the replacement tx measures its own
            // inclusion duration rather than inheriting from the original.
            event_log.pending_time = Some(Instant::now());
            event_log.fb_included = false;
            self.tx_nonce_slots.pop(&tx_hash);
            self.txs.put(replaced_by, event_log);

            self.emit_transaction_event(
                tx_hash,
                TxEvent::Replaced,
                event_index,
                TxpoolEventData {
                    replacement_hash: Some(replaced_by),
                    time_in_mempool: Some(time_in_mempool),
                    ..Default::default()
                },
            );
            Self::record_histogram(time_in_mempool, TxEvent::Replaced);
        }
    }

    fn track_nonce_slot(&mut self, tx_hash: TxHash, slot: NonceSlot) {
        self.tx_nonce_slots.put(tx_hash, slot);
        if !self.nonce_summaries.contains(&slot) {
            self.nonce_summaries.put(slot, NonceSummary::new());
        }
    }

    fn nonce_replacement(&mut self, slot: NonceSlot) {
        if let Some(summary) = self.nonce_summaries.get_mut(&slot) {
            summary.replacement_count += 1;
            Metrics::nonce_replacements().increment(1);
        }
    }

    fn nonce_completed(&mut self, tx_hash: &TxHash, event: &TxEvent, received_at: Instant) {
        let Some(slot) = self.tx_nonce_slots.pop(tx_hash) else {
            return;
        };
        let Some(summary) = self.nonce_summaries.pop(&slot) else {
            return;
        };
        if *event == TxEvent::BlockInclusion {
            let e2e_duration = received_at.duration_since(summary.first_seen);
            Metrics::e2e_inclusion_duration().record(e2e_duration.as_millis() as f64);
            Metrics::replacement_count().record(summary.replacement_count as f64);
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
        self.emit_transaction_event(
            *tx_hash,
            TxEvent::Overflowed,
            event_log.events.len(),
            TxpoolEventData {
                time_in_mempool: Some(event_log.mempool_time.elapsed()),
                ..Default::default()
            },
        );
        Self::record_histogram(event_log.mempool_time.elapsed(), TxEvent::Overflowed);
        true
    }

    /// Records a metrics histogram. We have to use `histogram!` here because it supports tags.
    fn record_histogram(time_in_mempool: Duration, event: TxEvent) {
        metrics::histogram!("reth_transaction_tracing_tx_event", "event" => event.to_string())
            .record(time_in_mempool.as_millis() as f64);
    }

    fn emit_transaction_event(
        &self,
        tx_hash: TxHash,
        txpool_event: TxEvent,
        event_index: usize,
        event_data: TxpoolEventData,
    ) {
        let Some(event_type) = transaction_event_type(txpool_event) else {
            return;
        };
        let mut data = Map::from_iter([
            ("event_source".to_string(), json!(Self::EVENT_SOURCE)),
            ("txpool_event".to_string(), json!(txpool_event.to_string())),
            ("event_index".to_string(), json!(event_index)),
        ]);

        if let Some(node_role) = &self.node_role {
            data.insert("node_role".to_string(), json!(node_role));
        }
        if let Some(pool) = event_data.pool {
            data.insert("pool".to_string(), json!(pool.as_str()));
        }
        if let Some(replacement_hash) = event_data.replacement_hash {
            data.insert("replacement_hash".to_string(), json!(format!("{replacement_hash:#x}")));
        }
        if let Some(duration) = event_data.time_in_mempool {
            data.insert("time_in_mempool_ms".to_string(), duration_ms_json(duration));
        }
        if let Some(overflow_reason) = event_data.overflow_reason {
            data.insert("overflow_reason".to_string(), json!(overflow_reason));
        }
        let event_time_ns = Local::now().timestamp_nanos_opt().unwrap_or_default();

        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: event_type,
            tx_hash: tx_hash,
            id: {
                "event_index" => event_index,
                "event_time" => event_time_ns,
            },
            data: data,
        );
    }
}

#[derive(Debug, Clone, Default)]
struct TxpoolEventData {
    pool: Option<Pool>,
    replacement_hash: Option<TxHash>,
    time_in_mempool: Option<Duration>,
    overflow_reason: Option<&'static str>,
}

impl Pool {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
        }
    }
}

const fn transaction_event_type(event: TxEvent) -> Option<TransactionEventType> {
    match event {
        TxEvent::Pending => Some(TransactionEventType::Pending),
        TxEvent::Queued => Some(TransactionEventType::Queued),
        TxEvent::Dropped => Some(TransactionEventType::Dropped),
        TxEvent::Replaced => Some(TransactionEventType::Replaced),
        // Inclusion is journaled by the builder, not the txpool tracer.
        TxEvent::BlockInclusion => None,
        TxEvent::PendingToQueued => Some(TransactionEventType::PendingToQueued),
        TxEvent::QueuedToPending => Some(TransactionEventType::QueuedToPending),
        TxEvent::Overflowed => Some(TransactionEventType::Overflowed),
    }
}

fn duration_ms_json(duration: Duration) -> Value {
    json!(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::ops::Deref;

    use alloy_primitives::Address;
    use base_flashblocks::FlashblocksAPI;
    use base_flashblocks_node::test_harness::{FlashblockBuilder, FlashblocksBuilderTestHarness};
    use base_test_utils::Account;
    use tokio::time;

    use super::*;

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
    fn maps_txpool_events_to_shared_transaction_event_types() {
        assert_eq!(transaction_event_type(TxEvent::Pending), Some(TransactionEventType::Pending));
        assert_eq!(transaction_event_type(TxEvent::Queued), Some(TransactionEventType::Queued));
        assert_eq!(transaction_event_type(TxEvent::Dropped), Some(TransactionEventType::Dropped));
        assert_eq!(transaction_event_type(TxEvent::Replaced), Some(TransactionEventType::Replaced));
        assert_eq!(transaction_event_type(TxEvent::BlockInclusion), None);
        assert_eq!(
            transaction_event_type(TxEvent::PendingToQueued),
            Some(TransactionEventType::PendingToQueued)
        );
        assert_eq!(
            transaction_event_type(TxEvent::QueuedToPending),
            Some(TransactionEventType::QueuedToPending)
        );
        assert_eq!(
            transaction_event_type(TxEvent::Overflowed),
            Some(TransactionEventType::Overflowed)
        );
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
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion, Instant::now());

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
        tracker.transaction_completed(tx_hash, TxEvent::Dropped, Instant::now());

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
        let original_pending_time = tracker.txs.get(&tx_hash).unwrap().pending_time;
        assert_eq!(tracker.txs.len(), 1);

        std::thread::sleep(Duration::from_millis(1));

        // Replace transaction
        tracker.transaction_replaced(tx_hash, replacement_hash);

        // Original should be gone, replacement should exist
        assert!(tracker.txs.get(&tx_hash).is_none());
        assert!(tracker.txs.get(&replacement_hash).is_some());

        let event_log = tracker.txs.get(&replacement_hash).unwrap();
        assert_eq!(event_log.events.len(), 2);
        assert_eq!(event_log.events[0].1, TxEvent::Pending);
        assert_eq!(event_log.events[1].1, TxEvent::Replaced);

        // pending_time should be reset, not inherited from original
        assert!(event_log.pending_time.unwrap() > original_pending_time.unwrap());
        assert!(!event_log.fb_included);
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
    fn test_pending_time_resets_on_re_promotion() {
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

        std::thread::sleep(Duration::from_millis(1));

        tracker.transaction_moved(tx_hash, Pool::Pending);
        let second_pending_time = tracker.txs.get(&tx_hash).unwrap().pending_time;

        assert!(second_pending_time.unwrap() > first_pending_time.unwrap());
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
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion, Instant::now());
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
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion, Instant::now());
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
        tracker.transaction_completed(tx_hash1, TxEvent::BlockInclusion, Instant::now());

        // Only one should remain
        assert_eq!(tracker.txs.len(), 1);
        assert!(tracker.txs.get(&tx_hash2).is_some());
    }

    #[test]
    fn test_fb_included_resets_on_re_promotion() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // Mark as fb-included
        tracker.transaction_fb_included(tx_hash, Instant::now());
        assert!(tracker.txs.get(&tx_hash).unwrap().fb_included);

        // Demote to queued, then re-promote
        tracker.transaction_moved(tx_hash, Pool::Queued);
        tracker.transaction_moved(tx_hash, Pool::Pending);

        // fb_included should be reset so the new pending stint gets measured
        assert!(!tracker.txs.get(&tx_hash).unwrap().fb_included);
    }

    #[test]
    fn test_nonce_tracking_simple_inclusion() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();
        let sender = Address::random();
        let nonce = 42u64;
        let slot = NonceSlot::new(sender, nonce);

        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        tracker.track_nonce_slot(tx_hash, slot);

        assert!(tracker.nonce_summaries.contains(&slot));
        assert!(tracker.tx_nonce_slots.contains(&tx_hash));

        tracker.nonce_completed(&tx_hash, &TxEvent::BlockInclusion, Instant::now());

        assert!(!tracker.nonce_summaries.contains(&slot));
        assert!(!tracker.tx_nonce_slots.contains(&tx_hash));
    }

    #[test]
    fn test_nonce_tracking_with_replacement() {
        let mut tracker = Tracker::new(false);
        let original_hash = TxHash::random();
        let replacement_hash = TxHash::random();
        let sender = Address::random();
        let nonce = 7u64;
        let slot = NonceSlot::new(sender, nonce);

        tracker.transaction_inserted(original_hash, TxEvent::Pending);
        tracker.track_nonce_slot(original_hash, slot);

        tracker.transaction_replaced(original_hash, replacement_hash);
        tracker.nonce_replacement(slot);
        tracker.track_nonce_slot(replacement_hash, slot);

        let summary = tracker.nonce_summaries.get(&slot).unwrap();
        assert_eq!(summary.replacement_count, 1);

        // Original hash slot mapping should be gone (overwritten by replacement)
        assert_eq!(*tracker.tx_nonce_slots.get(&replacement_hash).unwrap(), slot);

        tracker.nonce_completed(&replacement_hash, &TxEvent::BlockInclusion, Instant::now());
        assert!(!tracker.nonce_summaries.contains(&slot));
    }

    #[test]
    fn test_fb_inclusion_recorded_only_once() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        let first_received_at = Instant::now();

        // First flashblock notification should mark the tx as fb-included.
        tracker.transaction_fb_included(tx_hash, first_received_at);
        let event_log = tracker.txs.get(&tx_hash).expect("tx should still be in cache");
        assert!(event_log.fb_included, "should be marked as fb-included after first call");
        assert_eq!(event_log.events.len(), 1);
        assert_eq!(event_log.events[0].1, TxEvent::Pending);

        // Simulate a later flashblock arriving — received_at is much later.
        let later_received_at = first_received_at + Duration::from_millis(500);
        tracker.transaction_fb_included(tx_hash, later_received_at);

        // The tx should still be present and still marked — the second call
        // must have been a no-op (no duplicate metric recording).
        let event_log = tracker.txs.get(&tx_hash).expect("tx should still be in cache");
        assert!(event_log.fb_included);
    }

    #[tokio::test]
    async fn test_fb_inclusion() -> eyre::Result<()> {
        // Setup
        let harness = FlashblocksBuilderTestHarness::new().await;
        let mut tracker = Tracker::new(false);
        harness.send_flashblock(FlashblockBuilder::new_base(&harness).build()).await;

        // Build transaction & flashblock
        let tx = harness.build_transaction_to_send_eth_with_nonce(
            Account::Alice,
            Account::Bob,
            1000000000000000000,
            0,
        );
        let tx_hash = *tx.hash();
        let fb = FlashblockBuilder::new(&harness, 1).with_transactions(vec![tx]).build();

        // Mimic sending a tx to the mpool/builder
        tracker.transaction_inserted(tx_hash, TxEvent::Pending);

        // Wait a bit to simulate builder picking and building the tx into the pending block
        time::sleep(Duration::from_millis(10)).await;
        harness.node.send_flashblock(fb).await?;

        let state = harness.flashblocks.get_pending_blocks();
        // Verify we have some pending transactions
        let ptxs = state.as_ref().map(|pb| pb.get_pending_transaction_hashes()).unwrap_or_default();
        assert_eq!(ptxs.len(), 2); // L1Info + tx
        assert_eq!(ptxs[1], tx_hash);

        let pb = state.as_ref().unwrap().deref();
        tracker.track_flashblock_transactions(pb, Instant::now());

        // It should still be in the tracker
        assert!(tracker.txs.get(&tx_hash).is_some());

        // Wait until its included in canonical block
        time::sleep(Duration::from_millis(1500)).await;
        tracker.transaction_completed(tx_hash, TxEvent::BlockInclusion, Instant::now());

        // It should be removed from the tracker
        assert!(tracker.txs.get(&tx_hash).is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_can_receive_fb() -> eyre::Result<()> {
        // Setup
        let harness = FlashblocksBuilderTestHarness::new().await;
        let mut tracker = Tracker::new(false);
        harness.send_flashblock(FlashblockBuilder::new_base(&harness).build()).await;

        // Subscribe to flashblocks
        let mut stream = harness.flashblocks.subscribe_to_flashblocks();
        let mut t = tracker.clone();

        // Use a oneshot channel to signal when we receive a flashblock
        let (tx_signal, rx_signal) = tokio::sync::oneshot::channel();
        let mut tx_signal = Some(tx_signal);

        tokio::spawn(async move {
            while let Ok(pending_blocks) = stream.recv().await {
                t.handle_flashblock_notification(pending_blocks, Instant::now());
                // Signal that we received a flashblock
                if let Some(signal) = tx_signal.take() {
                    let _ = signal.send(());
                }
            }
        });

        // Create a tx and flashblock
        let tx = harness.build_transaction_to_send_eth_with_nonce(
            Account::Alice,
            Account::Bob,
            1000000000000000000,
            0,
        );
        let tx_hash = *tx.hash();
        let fb = FlashblockBuilder::new(&harness, 1).with_transactions(vec![tx]).build();

        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
        // Send the flashblock
        harness.send_flashblock(fb).await;

        // Verify we received the flashblock by waiting for the signal
        tokio::time::timeout(std::time::Duration::from_secs(1), rx_signal)
            .await
            .expect("timeout waiting for flashblock")
            .expect("channel closed before receiving flashblock");

        Ok(())
    }
}
