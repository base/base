use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use alloy_consensus::BlockHeader;
use futures::StreamExt;
use reth_provider::CanonStateNotification;
use reth_transaction_pool::TransactionPool;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, trace, warn};

use crate::transaction::BundleTransaction;

/// Evicts expired bundle transactions from the pool on each new committed block
/// and keeps the shared `current_block_number` in sync with the chain tip.
///
/// Transactions with `max_timestamp` before the block timestamp or
/// `target_block_number` before the block number are removed.
/// Regular transactions (no bundle metadata) are never affected.
///
/// Intended to be spawned on both client and builder nodes with their
/// respective pool instances.
pub async fn maintain_bundle_transactions<P, N>(
    pool: P,
    mut events: BroadcastStream<CanonStateNotification<N>>,
    current_block_number: Arc<AtomicU64>,
) where
    P: TransactionPool + 'static,
    P::Transaction: BundleTransaction,
    N: reth_node_api::NodePrimitives,
{
    loop {
        let notification = match events.next().await {
            Some(Ok(notification)) => notification,
            Some(Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n))) => {
                warn!(
                    missed = n,
                    "canon state stream lagged, some blocks were not checked for bundle expiry"
                );
                continue;
            }
            None => break,
        };

        let tip = notification.tip();
        let block_number = tip.number();
        let block_timestamp = tip.timestamp();

        current_block_number.store(block_number, Ordering::Release);

        let expired: Vec<_> = pool
            .pooled_transactions()
            .iter()
            .filter(|tx| tx.transaction.is_bundle_expired(block_number, block_timestamp))
            .map(|tx| *tx.hash())
            .collect();

        if !expired.is_empty() {
            debug!(
                count = expired.len(),
                block_number = block_number,
                block_timestamp = block_timestamp,
                "evicting expired bundle transactions",
            );
            pool.remove_transactions(expired);
        } else {
            trace!(
                block_number = block_number,
                block_timestamp = block_timestamp,
                "no expired bundle transactions",
            );
        }
    }
}
