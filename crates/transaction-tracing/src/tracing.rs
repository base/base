use alloy_primitives::TxHash;
use eyre::Result;
use futures::StreamExt;
use lru::LruCache;
use reth::api::{BlockBody, FullNodeComponents};
use reth::core::primitives::{AlloyBlockHeader, SignedTransaction};
use reth::transaction_pool::{FullTransactionEvent, TransactionPool};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_tracing::tracing::{debug, info};
use std::num::NonZeroUsize;
use std::time::Instant;

/// Max size of the LRU cache
const MAX_SIZE: usize = 20000;

/// Types of transaction events to track
#[derive(Debug, Clone, Copy, PartialEq)]
enum TxEvent {
    Dropped,
    Replaced,
    Pending,
    Queued,
    BlockInclusion,
    PendingToQueued,
    QueuedToPending,
}

/// Types of pools a transaction can be in
#[derive(Debug, Clone, PartialEq)]
enum Pool {
    Pending,
    Queued,
}

/// History of events for a transaction
struct EventLog {
    mempool_time: Instant,
    events: Vec<TxEvent>,
    limit: usize,
}

/// ExEx that tracks transaction timing from mempool to inclusion
struct Tracker {
    /// Map of transaction hash to timestamp when first seen in mempool
    txs: LruCache<TxHash, EventLog>,
    /// Map of transaction hash to current state
    tx_states: LruCache<TxHash, Pool>,
}

impl Tracker {
    /// Create a new tracker
    fn new() -> Self {
        Self {
            txs: LruCache::new(NonZeroUsize::new(MAX_SIZE).unwrap()),
            tx_states: LruCache::new(NonZeroUsize::new(MAX_SIZE).unwrap()),
        }
    }

    fn log(&self, tx_hash: &TxHash, event_log: &EventLog, msg: &str) {
        let events_string = event_log
            .events
            .iter()
            .filter(|event| **event != TxEvent::QueuedToPending)
            .map(|event| format!("{:?}", event))
            .collect::<Vec<_>>()
            .join("\n");

        if !events_string.is_empty() {
            info!(target: "transaction-tracing", tx_hash = ?tx_hash, events = %events_string, %msg);
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
        if self.txs.len() == MAX_SIZE {
            if let Some((tx_hash, event_log)) = self.txs.peek_lru() {
                self.log(tx_hash, event_log, "Transaction inserted");
            }
        }

        self.txs.put(
            tx_hash,
            EventLog {
                mempool_time: Instant::now(),
                events: vec![event],
                limit: 10,
            },
        );
    }

    /// Track a transaction moving from one pool to another
    fn transaction_moved(&mut self, tx_hash: TxHash, pool: Pool) {
        // if we've seen the transaction pending or queued before, track the pending <> queue transition
        if let Some(prev_pool) = self.tx_states.get(&tx_hash) {
            if prev_pool != &pool {
                let event = match (prev_pool, &pool) {
                    (Pool::Pending, Pool::Queued) => Some(TxEvent::PendingToQueued),
                    (Pool::Queued, Pool::Pending) => Some(TxEvent::QueuedToPending),
                    _ => None,
                };
                if let Some(tx_event) = event {
                    self.transaction_event(tx_hash, tx_event);
                }
            }
        }

        // update the new pool the transaction is in
        self.tx_states.put(tx_hash, pool.clone());
        debug!(target: "transaction-tracing", tx_hash = ?tx_hash, state = ?pool, "Transaction moved pools");
    }

    /// Track a transaction being included in a block or dropped.
    fn transaction_completed(&mut self, tx_hash: TxHash, event: TxEvent) {
        // if a tx is included/dropped, log it and pop it so that we keep the LRU cache size small
        // which will help longer-lived txs.
        if let Some(event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = Instant::now().duration_since(mempool_time);

            self.log(
                &tx_hash,
                &event_log,
                match event {
                    TxEvent::BlockInclusion => "Transaction included in block",
                    TxEvent::Dropped => "Transaction dropped",
                    _ => "",
                },
            );
            metrics::histogram!("reth_transaction_tracing_tx_event", "event" => match event {
                TxEvent::BlockInclusion => "block_inclusion",
                TxEvent::Dropped => "dropped",
                _ => "",
            })
            .record(time_in_mempool.as_millis() as f64);
        }
    }

    /// Track a transaction event
    fn transaction_event(&mut self, tx_hash: TxHash, event: TxEvent) {
        if let Some(mut event_log) = self.txs.pop(&tx_hash) {
            let mempool_time = event_log.mempool_time;
            let time_in_mempool = Instant::now().duration_since(mempool_time);

            if event_log.events.len() >= event_log.limit {
                self.log(
                    &tx_hash,
                    &event_log,
                    "Transaction removed from cache due to limit",
                );
                // the tx is already removed from the cache on `pop`
                return;
            }

            // Update the event log & vec size
            event_log.events.push(event);
            event_log.limit += 1;
            self.txs.put(tx_hash, event_log);

            // Record histogram with event label
            let event_label = match event {
                TxEvent::Replaced => "replaced",
                TxEvent::Pending => "pending",
                TxEvent::Queued => "queued",
                TxEvent::PendingToQueued => "pending_to_queued",
                TxEvent::QueuedToPending => "queued_to_pending",
                _ => "",
            };
            metrics::histogram!("reth_transaction_tracing_tx_event", "event" => event_label)
                .record(time_in_mempool.as_millis() as f64);

            debug!(
                target: "transaction-tracing",
                tx_hash = ?tx_hash,
                event = ?event,
                time_in_mempool = ?time_in_mempool.as_millis(),
                "Transaction event",
            );
        } else {
            debug!(
                target: "transaction-tracing",
                tx_hash = ?tx_hash,
                "Transaction included in block (not tracked by ExEx)",
            );
        }
    }
}

pub async fn transaction_tracing_exex<Node: FullNodeComponents>(
    mut ctx: ExExContext<Node>,
) -> Result<()> {
    debug!(target: "transaction-tracing", "Starting transaction tracking ExEx");
    let mut track = Tracker::new();

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
                    FullTransactionEvent::Queued(tx_hash) => {
                        track.transaction_inserted(tx_hash, TxEvent::Queued);
                        track.transaction_moved(tx_hash, Pool::Queued);
                    }
                    FullTransactionEvent::Discarded(tx_hash) => {
                        track.transaction_completed(tx_hash, TxEvent::Dropped);
                    }
                    FullTransactionEvent::Replaced{transaction, replaced_by: _} => {
                        let tx_hash = transaction.hash();
                        track.transaction_event(*tx_hash, TxEvent::Replaced);
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
