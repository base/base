//! Tracex canonical block subscription.

use futures::StreamExt;
use reth_node_api::NodePrimitives;
use reth_provider::CanonStateNotification;
use reth_transaction_pool::TransactionPool;
use tokio_stream::wrappers::BroadcastStream;

use crate::tracker::Tracker;

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
            // Track # of transactions dropped and replaced.
            Some(full_event) = all_events_stream.next() => tracker.handle_event(full_event),

            // Use canonical state notifications to track time to inclusion.
            Some(Ok(notification)) = canonical_stream.next() => {
                tracker.handle_canon_state_notification(notification);
            }
        }
    }
}
