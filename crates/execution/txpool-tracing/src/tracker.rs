//! Transaction tracking state machine powering the tracex execution extension.

use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use chrono::Local;
use lru::LruCache;
use reth_node_api::{BlockBody, NodePrimitives};
use reth_primitives_traits::transaction::TxHashRef;
use reth_provider::{CanonStateNotification, Chain};
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{FullTransactionEvent, PoolTransaction};

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
}

impl Tracker {
    /// Max size of the LRU caches.
    pub const MAX_SIZE: usize = 20_000;

    /// Block inclusion duration above this threshold increments the slow counter.
    const SLOW_BLOCK_INCLUSION_THRESHOLD: Duration = Duration::from_secs(3);

    /// Create a new tracker.
    pub fn new(enable_logs: bool) -> Self {
        let cache_size = NonZeroUsize::new(Self::MAX_SIZE).expect("non zero");
        Self {
            txs: LruCache::new(cache_size),
            tx_states: LruCache::new(cache_size),
            tx_nonce_slots: LruCache::new(cache_size),
            nonce_summaries: LruCache::new(cache_size),
            enable_logs,
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

                // Reset pending_time when transitioning to pending so that
                // inclusion duration only measures time actually spent in the
                // pending subpool, not time spent in queued/basefee.
                if event == TxEvent::QueuedToPending {
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

            if event == TxEvent::BlockInclusion
                && let Some(pending_time) = event_log.pending_time
            {
                let time_pending_to_inclusion = received_at.duration_since(pending_time);
                Metrics::inclusion_duration().record(time_pending_to_inclusion.as_millis() as f64);

                if time_pending_to_inclusion > Self::SLOW_BLOCK_INCLUSION_THRESHOLD {
                    Metrics::slow_inclusions().increment(1);
                } else {
                    Metrics::healthy_inclusions().increment(1);
                }
            }

            self.nonce_completed(&tx_hash, &event, received_at);
            self.log(&tx_hash, &event_log, &format!("Transaction {event}"));
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
            // Reset pending_time so the replacement tx measures its own
            // inclusion duration rather than inheriting from the original.
            event_log.pending_time = Some(Instant::now());
            self.tx_nonce_slots.pop(&tx_hash);
            self.txs.put(replaced_by, event_log);

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
        Self::record_histogram(event_log.mempool_time.elapsed(), TxEvent::Overflowed);
        true
    }

    /// Records a metrics histogram. We have to use `histogram!` here because it supports tags.
    fn record_histogram(time_in_mempool: Duration, event: TxEvent) {
        metrics::histogram!("reth_transaction_tracing_tx_event", "event" => event.to_string())
            .record(time_in_mempool.as_millis() as f64);
    }
}
