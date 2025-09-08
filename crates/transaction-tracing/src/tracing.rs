use alloy_primitives::TxHash;
use eyre::Result;
use futures::StreamExt;
use reth::api::{BlockBody, FullNodeComponents};
use reth::core::primitives::{AlloyBlockHeader, SignedTransaction};
use reth::transaction_pool::{FullTransactionEvent, TransactionPool};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_tracing::tracing::info;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;

/// Simple ExEx that tracks transaction timing from mempool to inclusion
struct Tracker {
    /// Map of transaction hash to timestamp when first seen in mempool
    txs: HashMap<TxHash, Instant>,
}

impl Tracker {
    fn new() -> Self {
        Self {
            txs: HashMap::new(),
        }
    }

    fn mempool_tx(&mut self, tx_hash: TxHash) {
        let now = Instant::now();
        self.txs.entry(tx_hash).or_insert(now);
        info!(target: "transaction-tracing", "Transaction {} added to mempool", tx_hash);
    }

    fn block_inclusion(&mut self, tx_hash: TxHash, block_number: u64) {
        let inclusion_time = Instant::now();

        if let Some(mempool_time) = self.txs.remove(&tx_hash) {
            let time_in_mempool = inclusion_time.duration_since(mempool_time);

            info!(
                target: "transaction-tracing",
                tx_hash = ?tx_hash,
                block_number = ?block_number,
                time_in_mempool = ?time_in_mempool.as_millis(),
                "Transaction included in block",
            );
        } else {
            info!(
                target: "transaction-tracing",
                tx_hash = ?tx_hash,
                block_number = ?block_number,
                "Transaction included in block (not tracked by ExEx)",
            );
        }
    }

    // Handle transaction event (drop, invalid, etc.)
    fn handle_transaction_event(&mut self, tx_hash: TxHash, event: &str) {
        if let Some(mempool_time) = self.txs.remove(&tx_hash) {
            let time_in_mempool = Instant::now().duration_since(mempool_time);
            info!(
                target: "transaction-tracing",
                tx_hash = ?tx_hash,
                event = ?event,
                time_in_mempool = ?time_in_mempool.as_millis(),
                "Transaction event",
            );
        }
    }

    // Cleanup old entries in our map that have been sitting in the mempool for too long
    fn cleanup(&mut self) {
        const MAX_AGE: Duration = Duration::from_secs(300); // 5 minutes
        let now = Instant::now();

        let before_count = self.txs.len();
        self.txs
            .retain(|_, &mut timestamp| now.duration_since(timestamp) < MAX_AGE);

        let after_count = self.txs.len();
        if before_count > after_count {
            info!(target: "transaction-tracing", "Cleaned up {} old mempool entries", before_count - after_count);
        }
    }
}

pub async fn transaction_tracing_exex<Node: FullNodeComponents>(
    mut ctx: ExExContext<Node>,
) -> Result<()> {
    info!(target: "transaction-tracing", "Starting transaction tracking ExEx");

    let mut track = Tracker::new();

    // Subscribe to events from the mempool
    let pool = ctx.pool().clone();
    let mempool_receiver = pool.pending_transactions_listener();
    let mut mempool_stream = ReceiverStream::new(mempool_receiver);

    // Subscribe to all transaction events to detect drops/replacements
    let mut all_events_stream = pool.all_transactions_event_listener();

    loop {
        tokio::select! {
            // New events from the mempool
            Some(tx_hash) = mempool_stream.next() => {
                track.mempool_tx(tx_hash);
            }

            // Track # of transactions dropped and replaced
            Some(full_event) = all_events_stream.next() => {
                match full_event {
                    FullTransactionEvent::Discarded(tx_hash) => {
                        track.handle_transaction_event(tx_hash, "tx dropped");
                    }
                    FullTransactionEvent::Replaced{transaction, replaced_by: _} => {
                        let tx_hash = transaction.hash();
                        track.handle_transaction_event(*tx_hash, "tx replaced");
                    }
                    _ => {
                        // Other events (mined, replaced, etc.)
                    }
                }
            }

            // Use chain notifications to track time to inclusion
            Some(notification) = ctx.notifications.next() => {
                match notification {
                    Ok(ExExNotification::ChainCommitted { new }) => {
                        // Process all transactions in committed chain
                        for (block_number, block) in new.blocks() {
                            for transaction in block.body().transactions() {
                                track.block_inclusion(*transaction.tx_hash(), *block_number);
                            }
                        }
                        // Periodic cleanup
                        track.cleanup();
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReorged { old: _, new }) => {
                        info!(target: "transaction-tracing", "Chain reorg detected, tip: {}", new.tip().number());
                        for (block_number, block) in new.blocks() {
                            for transaction in block.body().transactions() {
                                track.block_inclusion(*transaction.tx_hash(), *block_number);
                            }
                        }
                        track.cleanup();
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReverted { old }) => {
                        info!(target: "transaction-tracing", "Chain reverted, old tip: {}", old.tip().number());
                        ctx.events.send(ExExEvent::FinishedHeight(old.tip().num_hash()))?;
                    }
                    Err(e) => {
                        info!(target: "transaction-tracing", "Notification error: {}", e);
                        return Err(e);
                    }
                }
            }
        }
    }
}
