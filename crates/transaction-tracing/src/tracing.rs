//! Transaction tracing execution extension.

use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use chrono::Local;
use eyre::Result;
use futures::StreamExt;
use lru::LruCache;
use reth::{
    api::{BlockBody, FullNodeComponents},
    core::primitives::{AlloyBlockHeader, transaction::TxHashRef},
    transaction_pool::{FullTransactionEvent, TransactionPool},
};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_tracing::tracing::{debug, info};

use crate::types::{EventLog, Pool, TxEvent};

/// Max size of the LRU cache
const MAX_SIZE: usize = 20000;

fn record_histogram(time_in_mempool: Duration, event: TxEvent) {
    metrics::histogram!("reth_transaction_tracing_tx_event", "event" => event.to_string())
        .record(time_in_mempool.as_millis() as f64);
}

/// ExEx that tracks transaction timing from mempool to inclusion
struct Tracker {
    /// Map of transaction hash to timestamp when first seen in mempool
    txs: LruCache<TxHash, EventLog>,
    /// Map of transaction hash to current state
    tx_states: LruCache<TxHash, Pool>,
    /// Enable `info` logs for transaction tracing
    enable_logs: bool,
}

impl Tracker {
    /// Create a new tracker
    fn new(enable_logs: bool) -> Self {
        Self {
            txs: LruCache::new(NonZeroUsize::new(MAX_SIZE).unwrap()),
            tx_states: LruCache::new(NonZeroUsize::new(MAX_SIZE).unwrap()),
            enable_logs,
        }
    }

    /// Track the first time we see a transaction in the mempool
    fn transaction_inserted(&mut self, tx_hash: TxHash, event: TxEvent) {
        // if we've seen the tx before, don't track it again. for example,
        // if a tx was pending then moved to queued, we don't want to update the timestamp
        // with the queued timestamp.
        if self.txs.contains(&tx_hash) {
            return;
        }

        // if the LRU is full and we're about to insert a new tx, log the `EventLog` for that tx
        // before it gets evicted. this can be useful to see the full history of a transaction.
        if self.txs.len() == MAX_SIZE
            && let Some((tx_hash, event_log)) = self.txs.peek_lru()
        {
            self.log(tx_hash, event_log, "Transaction inserted");
        }

        self.txs.put(tx_hash, EventLog::new(Local::now(), event));
    }

    /// Track a transaction moving from one pool to another
    fn transaction_moved(&mut self, tx_hash: TxHash, pool: Pool) {
        // if we've seen the transaction pending or queued before, track the pending <> queue transition
        if let Some(prev_pool) = self.tx_states.get(&tx_hash)
            && prev_pool != &pool
        {
            let event = match (prev_pool, &pool) {
                (Pool::Pending, Pool::Queued) => Some(TxEvent::PendingToQueued),
                (Pool::Queued, Pool::Pending) => Some(TxEvent::QueuedToPending),
                _ => None,
            };
            if event.is_none() {
                return;
            }

            if let Some(mut event_log) = self.txs.pop(&tx_hash) {
                let mempool_time = event_log.mempool_time;
                let time_in_mempool = Instant::now().duration_since(mempool_time);

                if self.is_overflowed(&tx_hash, &event_log) {
                    // the tx is already removed from the cache from `pop`
                    return;
                }
                event_log.push(Local::now(), event.unwrap());
                self.txs.put(tx_hash, event_log);

                record_histogram(time_in_mempool, event.unwrap());
            }
        }

        // update the new pool the transaction is in
        self.tx_states.put(tx_hash, pool.clone());
        debug!(target: "transaction-tracing", tx_hash = ?tx_hash, state = ?pool, "Transaction moved pools");
    }

    /// Track a transaction being included in a block or dropped.
    fn transaction_completed(&mut self, tx_hash: TxHash, event: TxEvent) {
        if let Some(mut event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = Instant::now().duration_since(mempool_time);

            if self.is_overflowed(&tx_hash, &event_log) {
                return;
            }
            // don't add it back to LRU so that we keep the LRU cache size small which will help longer-lived txs
            // but do update the event log with the final event (i.e., included/dropped)
            event_log.push(Local::now(), event);

            // if a tx is included/dropped, log it now.
            self.log(&tx_hash, &event_log, &format!("Transaction {event}"));
            record_histogram(time_in_mempool, event);
        }
    }

    /// Track a transaction being replaced by removing it from the cache and adding the new tx.
    fn transaction_replaced(&mut self, tx_hash: TxHash, replaced_by: TxHash) {
        if let Some(mut event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = Instant::now().duration_since(mempool_time);
            debug!(target: "transaction-tracing", tx_hash = ?tx_hash, replaced_by = ?replaced_by, "Transaction replaced");

            if self.is_overflowed(&tx_hash, &event_log) {
                return;
            }
            // keep the event log and update the tx hash
            event_log.push(Local::now(), TxEvent::Replaced);
            self.txs.put(replaced_by, event_log);

            record_histogram(time_in_mempool, TxEvent::Replaced);
        }
    }

    fn log(&self, tx_hash: &TxHash, event_log: &EventLog, msg: &str) {
        if !self.enable_logs {
            return;
        }

        let events = event_log.to_vec();
        if !events.is_empty() {
            info!(target: "transaction-tracing", tx_hash = ?tx_hash, events = ?events, %msg);
        }
    }

    // if `is_overflowed` is true then we record an overflowed metric and log the event log
    // and don't record the other event that was supposed to be recorded
    fn is_overflowed(&self, tx_hash: &TxHash, event_log: &EventLog) -> bool {
        if event_log.events.len() < event_log.limit {
            return false;
        }

        self.log(tx_hash, event_log, "Transaction removed from cache due to limit");
        record_histogram(event_log.mempool_time.elapsed(), TxEvent::Overflowed);
        true
    }
}

/// Execution extension that tracks transaction timing from mempool to inclusion.
///
/// Monitors transaction lifecycle events and records timing metrics.
pub async fn transaction_tracing_exex<Node: FullNodeComponents>(
    mut ctx: ExExContext<Node>,
    enable_logs: bool,
) -> Result<()> {
    debug!(target: "transaction-tracing", "Starting transaction tracking ExEx");
    let mut track = Tracker::new(enable_logs);

    // Subscribe to events from the mempool
    let pool = ctx.pool().clone();
    let mut all_events_stream = pool.all_transactions_event_listener();

    loop {
        tokio::select! {
            // Track # of transactions dropped and replaced
            Some(full_event) = all_events_stream.next() => {
                match full_event {
                    FullTransactionEvent::Pending(tx_hash) => {
                        track.transaction_inserted(tx_hash, TxEvent::Pending);
                        track.transaction_moved(tx_hash, Pool::Pending);
                    }
                    FullTransactionEvent::Queued(tx_hash, _) => {
                        track.transaction_inserted(tx_hash, TxEvent::Queued);
                        track.transaction_moved(tx_hash, Pool::Queued);
                    }
                    FullTransactionEvent::Discarded(tx_hash) => {
                        track.transaction_completed(tx_hash, TxEvent::Dropped);
                    }
                    FullTransactionEvent::Replaced{transaction, replaced_by} => {
                        let tx_hash = transaction.hash();
                        track.transaction_replaced(*tx_hash, TxHash::from(replaced_by));
                    }
                    _ => {
                        // Other events
                    }
                }
            }

            // Use chain notifications to track time to inclusion
            Some(notification) = ctx.notifications.next() => {
                match notification {
                    Ok(ExExNotification::ChainCommitted { new }) => {
                        // Process all transactions in committed chain
                        for block in new.blocks().values() {
                            for transaction in block.body().transactions() {
                                track.transaction_completed(*transaction.tx_hash(), TxEvent::BlockInclusion);
                            }
                        }
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReorged { old: _, new }) => {
                        debug!(target: "transaction-tracing", tip = ?new.tip().number(), "Chain reorg detected");
                        for block in new.blocks().values() {
                            for transaction in block.body().transactions() {
                                track.transaction_completed(*transaction.tx_hash(), TxEvent::BlockInclusion);
                            }
                        }
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReverted { old }) => {
                        debug!(target: "transaction-tracing", old_tip = ?old.tip().number(), "Chain reverted");
                        ctx.events.send(ExExEvent::FinishedHeight(old.tip().num_hash()))?;
                    }
                    Err(e) => {
                        debug!(target: "transaction-tracing", "Notification error: {}", e);
                        return Err(e);
                    }
                }
            }
        }
    }
}
