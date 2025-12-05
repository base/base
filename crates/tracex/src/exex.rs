//! Tracex execution extension wiring.

use alloy_primitives::TxHash;
use eyre::Result;
use futures::StreamExt;
use reth::{
    api::{BlockBody, FullNodeComponents},
    core::primitives::{AlloyBlockHeader, transaction::TxHashRef},
    transaction_pool::{FullTransactionEvent, TransactionPool},
};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_tracing::tracing::debug;

use crate::{
    events::{Pool, TxEvent},
    tracker::Tracker,
};

/// Execution extension that tracks transaction timing from mempool to inclusion.
///
/// Monitors transaction lifecycle events and records timing metrics.
pub async fn tracex_exex<Node: FullNodeComponents>(
    mut ctx: ExExContext<Node>,
    enable_logs: bool,
) -> Result<()> {
    debug!(target: "tracex", "Starting transaction tracking ExEx");
    let mut tracker = Tracker::new(enable_logs);

    // Subscribe to events from the mempool.
    let pool = ctx.pool().clone();
    let mut all_events_stream = pool.all_transactions_event_listener();

    loop {
        tokio::select! {
            // Track # of transactions dropped and replaced.
            Some(full_event) = all_events_stream.next() => {
                match full_event {
                    FullTransactionEvent::Pending(tx_hash) => {
                        tracker.transaction_inserted(tx_hash, TxEvent::Pending);
                        tracker.transaction_moved(tx_hash, Pool::Pending);
                    }
                    FullTransactionEvent::Queued(tx_hash, _) => {
                        tracker.transaction_inserted(tx_hash, TxEvent::Queued);
                        tracker.transaction_moved(tx_hash, Pool::Queued);
                    }
                    FullTransactionEvent::Discarded(tx_hash) => {
                        tracker.transaction_completed(tx_hash, TxEvent::Dropped);
                    }
                    FullTransactionEvent::Replaced{ transaction, replaced_by } => {
                        let tx_hash = transaction.hash();
                        tracker.transaction_replaced(*tx_hash, TxHash::from(replaced_by));
                    }
                    _ => {
                        // Other events.
                    }
                }
            }

            // Use chain notifications to track time to inclusion.
            Some(notification) = ctx.notifications.next() => {
                match notification {
                    Ok(ExExNotification::ChainCommitted { new }) => {
                        // Process all transactions in committed chain.
                        for block in new.blocks().values() {
                            for transaction in block.body().transactions() {
                                tracker.transaction_completed(*transaction.tx_hash(), TxEvent::BlockInclusion);
                            }
                        }
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReorged { old: _, new }) => {
                        debug!(target: "tracex", tip = ?new.tip().number(), "Chain reorg detected");
                        for block in new.blocks().values() {
                            for transaction in block.body().transactions() {
                                tracker.transaction_completed(*transaction.tx_hash(), TxEvent::BlockInclusion);
                            }
                        }
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                    }
                    Ok(ExExNotification::ChainReverted { old }) => {
                        debug!(target: "tracex", old_tip = ?old.tip().number(), "Chain reverted");
                        ctx.events.send(ExExEvent::FinishedHeight(old.tip().num_hash()))?;
                    }
                    Err(e) => {
                        debug!(target: "tracex", error = %e, "Notification error");
                        return Err(e);
                    }
                }
            }
        }
    }
}
