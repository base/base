use alloy_primitives::TxHash;
use eyre::Result;
use futures::StreamExt;
use lru::LruCache;
use reth::api::{BlockBody, FullNodeComponents};
use reth::core::primitives::{AlloyBlockHeader, SignedTransaction};
use reth::transaction_pool::{FullTransactionEvent, TransactionPool};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_tracing::tracing::debug;
use std::num::NonZeroUsize;
use std::time::Instant;

/// Types of transaction events to track
#[derive(Debug)]
enum TxEvent {
    Dropped,
    Replaced,
    BlockInclusion,
    PendingToQueued,
    QueuedToPending,
}

/// Types of pools a transaction can be in
#[derive(Debug, Clone, PartialEq)]
enum Pools {
    Pending,
    Queued,
}

/// Simple ExEx that tracks transaction timing from mempool to inclusion
struct Tracker {
    /// Map of transaction hash to timestamp when first seen in mempool
    txs: LruCache<TxHash, Instant>,
    /// Map of transaction hash to current state
    tx_states: LruCache<TxHash, Pools>,
}

impl Tracker {
    /// Create a new tracker
    fn new() -> Self {
        Self {
            txs: LruCache::new(NonZeroUsize::new(20000).unwrap()),
            tx_states: LruCache::new(NonZeroUsize::new(20000).unwrap()),
        }
    }

    /// Track the first time we see a transaction in the mempool
    fn transaction_inserted(&mut self, tx_hash: TxHash) {
        // if we've seen the tx before, don't track it again. for example,
        // if a tx was pending then moved to queued, we don't want to update the timestamp
        // with the queued timestamp.
        if self.txs.contains(&tx_hash) {
            return;
        }

        let now = Instant::now();
        self.txs.put(tx_hash, now);
    }

    /// Track a transaction moving from one pool to another
    fn transaction_moved(&mut self, tx_hash: TxHash, pool: Pools) {
        // if we've seen the transaction pending or queued before, track the pending <> queue transition
        if let Some(prev_pool) = self.tx_states.get(&tx_hash) {
            if prev_pool != &pool {
                let event = match (prev_pool, &pool) {
                    (Pools::Pending, Pools::Queued) => Some(TxEvent::PendingToQueued),
                    (Pools::Queued, Pools::Pending) => Some(TxEvent::QueuedToPending),
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

    /// Track a transaction event
    fn transaction_event(&mut self, tx_hash: TxHash, event: TxEvent) {
        if let Some(mempool_time) = self.txs.pop(&tx_hash) {
            let time_in_mempool = Instant::now().duration_since(mempool_time);

            // Record histogram with event label
            let event_label = match event {
                TxEvent::Dropped => "dropped",
                TxEvent::Replaced => "replaced",
                TxEvent::BlockInclusion => "block_inclusion",
                TxEvent::PendingToQueued => "pending_to_queued",
                TxEvent::QueuedToPending => "queued_to_pending",
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
                        track.transaction_inserted(tx_hash);
                        track.transaction_moved(tx_hash, Pools::Pending);
                    }
                    FullTransactionEvent::Queued(tx_hash) => {
                        track.transaction_inserted(tx_hash);
                        track.transaction_moved(tx_hash, Pools::Queued);
                    }
                    FullTransactionEvent::Discarded(tx_hash) => {
                        track.transaction_event(tx_hash, TxEvent::Dropped);
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
                                track.transaction_event(*transaction.tx_hash(), TxEvent::BlockInclusion);
                            }
                        }
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReorged { old: _, new }) => {
                        debug!(target: "transaction-tracing", tip = ?new.tip().number(), "Chain reorg detected");
                        for block in new.blocks().values() {
                            for transaction in block.body().transactions() {
                                track.transaction_event(*transaction.tx_hash(), TxEvent::BlockInclusion);
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
