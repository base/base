//! Tracex subscription wiring.

use futures::StreamExt;
use reth::{providers::CanonStateSubscriptions, transaction_pool::TransactionPool};
use reth_tracing::tracing::{debug, warn};
use tokio::sync::broadcast::error::RecvError;

use crate::tracker::Tracker;

/// Starts a background task that tracks transaction timing from mempool to inclusion.
///
/// Monitors transaction lifecycle events and records timing metrics.
pub fn start_tracex<Pool, Provider>(pool: Pool, provider: Provider, enable_logs: bool)
where
    Pool: TransactionPool + 'static,
    Provider: CanonStateSubscriptions + 'static,
{
    debug!(target: "tracex", "Starting transaction tracking subscription");
    let mut tracker = Tracker::new(enable_logs);

    // Subscribe to events from the mempool.
    let mut all_events_stream = pool.all_transactions_event_listener();

    // Subscribe to canonical state notifications.
    let mut canon_receiver = provider.subscribe_to_canonical_state();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Track # of transactions dropped and replaced.
                Some(full_event) = all_events_stream.next() => tracker.handle_event(full_event),

                // Use chain notifications to track time to inclusion.
                result = canon_receiver.recv() => {
                    match result {
                        Ok(notification) => tracker.handle_notification(notification),
                        Err(RecvError::Lagged(n)) => {
                            warn!(target: "tracex", missed = n, "Canonical subscription lagged");
                        }
                        Err(RecvError::Closed) => {
                            warn!(target: "tracex", "Canonical state subscription ended");
                            break;
                        }
                    }
                }
            }
        }
    });
}
