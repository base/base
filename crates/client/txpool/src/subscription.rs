//! Tracex canonical block subscription.

use std::sync::Arc;

use alloy_primitives::TxHash;
use base_flashblocks::{FlashblocksAPI, PendingBlocks};
use futures::StreamExt;
use reth_node_api::NodePrimitives;
use reth_provider::CanonStateNotification;
use reth_tracing::tracing::{debug, error};
use reth_transaction_pool::TransactionPool;
use tokio::sync::{broadcast::Receiver, mpsc::Sender};
use tokio_stream::wrappers::BroadcastStream;

use crate::tracker::Tracker;

/// Subscription task that tracks transaction timing from mempool to block inclusion.
///
/// Monitors transaction lifecycle events and records timing metrics by listening
/// to canonical state notifications, flashblock updates, and mempool events.
pub async fn tracex_subscription<N, Pool, FB>(
    canonical_stream: BroadcastStream<CanonStateNotification<N>>,
    flashblocks_api: Arc<FB>,
    pool: Pool,
    enable_logs: bool,
    tx_send: Sender<(TxHash, Vec<String>)>,
) where
    N: NodePrimitives,
    Pool: TransactionPool + 'static,
    FB: FlashblocksAPI + 'static,
{
    debug!(target: "tracex", "Starting transaction tracking subscription");
    let mut tracker = Tracker::new(enable_logs, tx_send);

    // Subscribe to events from the mempool.
    let mut all_events_stream = pool.all_transactions_event_listener();
    let mut canonical_stream = canonical_stream;

    // Subscribe to flashblocks.
    let mut flashblock_stream: Receiver<Arc<PendingBlocks>> =
        flashblocks_api.subscribe_to_flashblocks();

    loop {
        tokio::select! {
            // Track # of transactions dropped and replaced.
            Some(full_event) = all_events_stream.next() => {
                if let Err(e) = tracker.handle_event(full_event).await {
                    error!(target: "tracex", "Error handling transaction event: {}", e);
                }
            },

            // Use canonical state notifications to track time to inclusion.
            Some(Ok(notification)) = canonical_stream.next() => {
                if let Err(e) = tracker.handle_canon_state_notification(notification).await {
                    error!(target: "tracex", "Error handling canonical state notification: {}", e);
                }
            }

            // Track flashblock inclusion timing.
            Ok(pending_blocks) = flashblock_stream.recv() => {
                tracker.handle_flashblock_notification(pending_blocks);
            }
        }
    }
}
