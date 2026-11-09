//! Transaction tracking state machine powering the tracex execution extension.

use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
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

use crate::{
    EventLog, NonceSlot, NonceSummary, Pool, PooledTxInfo, TxEvent, TxFlow, metrics::Metrics,
};

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
    /// `info` is populated by the subscription layer for events that only
    /// carry a [`TxHash`] (Pending, Queued) by looking up the pool.
    pub fn handle_event<T: PoolTransaction>(
        &mut self,
        event: FullTransactionEvent<T>,
        info: Option<PooledTxInfo>,
    ) {
        match event {
            FullTransactionEvent::Pending(tx_hash) => {
                self.transaction_inserted(tx_hash, TxEvent::Pending);
                self.transaction_moved(tx_hash, Pool::Pending);
                if let Some(info) = info {
                    self.track_pooled_tx_info(tx_hash, info);
                }
            }
            FullTransactionEvent::Queued(tx_hash, _) => {
                self.transaction_inserted(tx_hash, TxEvent::Queued);
                self.transaction_moved(tx_hash, Pool::Queued);
                if let Some(info) = info {
                    self.track_pooled_tx_info(tx_hash, info);
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
        // Eviction is a tracker bookkeeping limit, not a transaction lifecycle fact, so it is
        // recorded as a metric and log only; it is not journaled as a transaction event.
        if self.txs.len() == Self::MAX_SIZE
            && let Some((tx_hash, event_log)) = self.txs.peek_lru()
        {
            self.log(tx_hash, event_log, "Transaction inserted");
            Self::record_histogram(event_log.mempool_time.elapsed(), TxEvent::Overflowed);
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
            let flow = event_log.flow;

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
                Metrics::inclusion_duration(flow.as_str())
                    .record(time_pending_to_inclusion.as_millis() as f64);

                if time_pending_to_inclusion > Self::SLOW_BLOCK_INCLUSION_THRESHOLD {
                    Metrics::slow_inclusions(flow.as_str()).increment(1);
                } else {
                    Metrics::healthy_inclusions(flow.as_str()).increment(1);
                }
            }

            self.nonce_completed(&tx_hash, &event, flow, received_at);
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
            // The replacement may already be tracked if the pool reported it first; its flow
            // was resolved from the replacement itself and takes precedence.
            if let Some(replacement) = self.txs.peek(&replaced_by) {
                event_log.flow = replacement.flow;
            }
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

    /// Records the pool metadata of a pending or queued transaction.
    fn track_pooled_tx_info(&mut self, tx_hash: TxHash, info: PooledTxInfo) {
        self.set_flow(tx_hash, info.flow);
        self.track_nonce_slot(tx_hash, info.nonce_slot);
    }

    /// Sets the flow used to label the inclusion metrics of a tracked transaction.
    pub fn set_flow(&mut self, tx_hash: TxHash, flow: TxFlow) {
        if let Some(event_log) = self.txs.peek_mut(&tx_hash) {
            event_log.flow = flow;
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

    fn nonce_completed(
        &mut self,
        tx_hash: &TxHash,
        event: &TxEvent,
        flow: TxFlow,
        received_at: Instant,
    ) {
        let Some(slot) = self.tx_nonce_slots.pop(tx_hash) else {
            return;
        };
        let Some(summary) = self.nonce_summaries.pop(&slot) else {
            return;
        };
        if *event == TxEvent::BlockInclusion {
            let e2e_duration = received_at.duration_since(summary.first_seen);
            Metrics::e2e_inclusion_duration(flow.as_str()).record(e2e_duration.as_millis() as f64);
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
    // and don't record the other event that was supposed to be recorded. Overflow describes
    // the tracker's own per-tx event limit, so it is not journaled as a transaction event.
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
        // Inclusion is journaled by the builder, not the txpool tracer. Overflow is a tracker
        // bookkeeping limit recorded as a metric and log only.
        TxEvent::BlockInclusion | TxEvent::Overflowed => None,
        TxEvent::PendingToQueued => Some(TransactionEventType::PendingToQueued),
        TxEvent::QueuedToPending => Some(TransactionEventType::QueuedToPending),
    }
}

fn duration_ms_json(duration: Duration) -> Value {
    json!(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_observability_events::TransactionEventCapture;
    use reth_transaction_pool::test_utils::MockTransaction;
    use rstest::rstest;

    use super::*;

    enum Action {
        Insert(TxEvent),
        MoveTo(Pool),
    }

    #[rstest]
    #[case::inserted_pending(
        &[(Action::Insert(TxEvent::Pending), true)],
        &[TxEvent::Pending]
    )]
    #[case::inserted_queued(
        &[(Action::Insert(TxEvent::Queued), false)],
        &[TxEvent::Queued]
    )]
    #[case::duplicate_insert_ignored(
        &[(Action::Insert(TxEvent::Pending), true), (Action::Insert(TxEvent::Queued), true)],
        &[TxEvent::Pending]
    )]
    #[case::moved_queued_to_pending(
        &[
            (Action::Insert(TxEvent::Queued), false),
            (Action::MoveTo(Pool::Queued), false),
            (Action::MoveTo(Pool::Pending), true),
        ],
        &[TxEvent::Queued, TxEvent::QueuedToPending]
    )]
    fn records_transaction_event_log(
        #[case] steps: &[(Action, bool)],
        #[case] expected_events: &[TxEvent],
    ) {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();

        for (action, expect_pending_time) in steps {
            match action {
                Action::Insert(event) => {
                    let mempool_time = tracker.txs.peek(&tx_hash).map(|log| log.mempool_time);
                    tracker.transaction_inserted(tx_hash, *event);
                    if let Some(mempool_time) = mempool_time {
                        let event_log = tracker.txs.peek(&tx_hash).expect("tx should exist");
                        assert_eq!(event_log.mempool_time, mempool_time);
                    }
                }
                Action::MoveTo(pool) => tracker.transaction_moved(tx_hash, pool.clone()),
            }

            assert_eq!(
                tracker.txs.peek(&tx_hash).expect("tx should exist").pending_time.is_some(),
                *expect_pending_time,
            );
        }

        assert_eq!(tracker.txs.len(), 1);
        let event_log = tracker.txs.peek(&tx_hash).expect("tx should exist");
        let events: Vec<TxEvent> = event_log.events.iter().map(|(_, event)| *event).collect();
        assert_eq!(events, expected_events);
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
        assert_eq!(transaction_event_type(TxEvent::Overflowed), None);
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
    fn per_tx_event_limit_overflow_records_metric_without_journal_event() {
        let event_capture = TransactionEventCapture::install();
        let tx_hash = TxHash::random();

        let rendered = render_metrics(|| {
            let mut tracker = Tracker::new(false);
            tracker.transaction_inserted(tx_hash, TxEvent::Pending);
            tracker.transaction_moved(tx_hash, Pool::Pending);

            // Fill the per-tx event log up to its limit.
            let mut event_log = tracker.txs.pop(&tx_hash).expect("tx should exist");
            while event_log.events.len() < event_log.limit {
                event_log.push(Local::now(), TxEvent::PendingToQueued);
            }
            tracker.txs.put(tx_hash, event_log);

            // The next transition overflows the log and evicts the tx.
            tracker.transaction_moved(tx_hash, Pool::Queued);
            assert!(tracker.txs.get(&tx_hash).is_none());
        });

        assert!(
            rendered.contains("reth_transaction_tracing_tx_event_count{event=\"overflowed\"} 1"),
            "expected overflowed histogram, got: {rendered}"
        );
        let event_types = event_capture
            .events()
            .into_iter()
            .filter(|event| event.tx_hash == Some(tx_hash))
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(event_types, [TransactionEventType::Pending]);
    }

    #[test]
    fn lru_eviction_records_metric_without_journal_event() {
        let event_capture = TransactionEventCapture::install();
        let evicted = TxHash::random();

        let rendered = render_metrics(|| {
            let mut tracker = Tracker::new(false);
            tracker.transaction_inserted(evicted, TxEvent::Pending);
            for _ in 1..Tracker::MAX_SIZE {
                tracker.transaction_inserted(TxHash::random(), TxEvent::Pending);
            }
            assert_eq!(tracker.txs.len(), Tracker::MAX_SIZE);

            // Inserting one more evicts the least-recently-used tx.
            tracker.transaction_inserted(TxHash::random(), TxEvent::Pending);
            assert!(tracker.txs.peek(&evicted).is_none());
        });

        assert!(
            rendered.contains("reth_transaction_tracing_tx_event_count{event=\"overflowed\"} 1"),
            "expected overflowed histogram, got: {rendered}"
        );
        let event_types = event_capture
            .events()
            .into_iter()
            .filter(|event| event.tx_hash == Some(evicted))
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        assert_eq!(event_types, [TransactionEventType::Pending]);
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

        tracker.nonce_completed(
            &tx_hash,
            &TxEvent::BlockInclusion,
            TxFlow::Standard,
            Instant::now(),
        );

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

        tracker.nonce_completed(
            &replacement_hash,
            &TxEvent::BlockInclusion,
            TxFlow::Standard,
            Instant::now(),
        );
        assert!(!tracker.nonce_summaries.contains(&slot));
    }

    fn render_metrics(f: impl FnOnce()) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    fn pending_event(tx_hash: TxHash) -> FullTransactionEvent<MockTransaction> {
        FullTransactionEvent::Pending(tx_hash)
    }

    #[rstest]
    #[case::standard(TxFlow::Standard, "standard")]
    #[case::validity(TxFlow::Validity, "validity")]
    fn inclusion_metrics_are_labeled_by_flow(#[case] flow: TxFlow, #[case] label: &str) {
        let rendered = render_metrics(|| {
            let mut tracker = Tracker::new(false);
            let tx_hash = TxHash::random();
            let nonce_slot = NonceSlot::new(Address::random(), 0);
            tracker.handle_event(pending_event(tx_hash), Some(PooledTxInfo { nonce_slot, flow }));

            tracker.transaction_completed(
                tx_hash,
                TxEvent::BlockInclusion,
                Instant::now() + Duration::from_secs(4),
            );
        });

        for metric in [
            "reth_transaction_tracing_slow_inclusions",
            "reth_transaction_tracing_inclusion_duration_count",
            "reth_transaction_tracing_e2e_inclusion_duration_count",
        ] {
            assert!(
                rendered.contains(&format!("{metric}{{flow=\"{label}\"}} 1")),
                "expected {metric} with flow={label}, got: {rendered}"
            );
        }
    }

    #[test]
    fn transactions_default_to_standard_flow() {
        let mut tracker = Tracker::new(false);
        let tx_hash = TxHash::random();
        tracker.handle_event(pending_event(tx_hash), None);

        assert_eq!(
            tracker.txs.peek(&tx_hash).expect("tx should be tracked").flow,
            TxFlow::Standard
        );
    }

    #[rstest]
    #[case::replacement_reported_first(true)]
    #[case::replacement_reported_later(false)]
    fn replacement_uses_its_own_flow(#[case] replacement_reported_first: bool) {
        let mut tracker = Tracker::new(false);
        let original = TxHash::random();
        let replacement = TxHash::random();
        let nonce_slot = NonceSlot::new(Address::random(), 0);
        let validity = Some(PooledTxInfo { nonce_slot, flow: TxFlow::Validity });
        tracker.handle_event(pending_event(original), validity);

        let standard = Some(PooledTxInfo { nonce_slot, flow: TxFlow::Standard });
        if replacement_reported_first {
            tracker.handle_event(pending_event(replacement), standard);
            tracker.transaction_replaced(original, replacement);
        } else {
            tracker.transaction_replaced(original, replacement);
            tracker.handle_event(pending_event(replacement), standard);
        }

        assert_eq!(
            tracker.txs.peek(&replacement).expect("replacement should be tracked").flow,
            TxFlow::Standard
        );
    }
}
