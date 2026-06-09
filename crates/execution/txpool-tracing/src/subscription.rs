//! Transaction tracing canonical block subscription.

use std::time::Instant;

use futures::StreamExt;
use reth_node_api::NodePrimitives;
use reth_provider::CanonStateNotification;
use reth_tracing::tracing::debug;
use reth_transaction_pool::{FullTransactionEvent, TransactionPool};
use tokio_stream::wrappers::BroadcastStream;

use crate::{NonceSlot, tracker::Tracker};

/// Subscription task that tracks transaction timing from mempool to block inclusion.
///
/// Monitors transaction lifecycle events and records timing metrics by listening
/// to canonical state notifications and mempool events.
pub async fn tracex_subscription<N, Pool>(
    canonical_stream: BroadcastStream<CanonStateNotification<N>>,
    pool: Pool,
    enable_logs: bool,
) where
    N: NodePrimitives,
    Pool: TransactionPool + 'static,
{
    debug!(target: "tracex", "Starting transaction tracking subscription");
    let mut tracker = Tracker::new(enable_logs);

    // Subscribe to events from the mempool.
    let mut all_events_stream = pool.all_transactions_event_listener();
    let mut canonical_stream = canonical_stream;

    loop {
        tokio::select! {
            Some(full_event) = all_events_stream.next() => {
                let nonce_slot = resolve_nonce_slot(&full_event, &pool);
                tracker.handle_event(full_event, nonce_slot);
            },

            // Use canonical state notifications to track time to inclusion.
            Some(Ok(notification)) = canonical_stream.next() => {
                let received_at = Instant::now();
                tracker.handle_canon_state_notification(notification, received_at);
            }
        }
    }
}

fn resolve_nonce_slot<Pool: TransactionPool>(
    event: &FullTransactionEvent<Pool::Transaction>,
    pool: &Pool,
) -> Option<NonceSlot> {
    let tx_hash = match event {
        FullTransactionEvent::Pending(hash) | FullTransactionEvent::Queued(hash, _) => hash,
        _ => return None,
    };
    pool.get(tx_hash).map(|tx| NonceSlot::new(tx.sender(), tx.nonce()))
}
