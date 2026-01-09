//! Tracex execution extension wiring.

use eyre::Result;
use futures::StreamExt;
use reth::{api::FullNodeComponents, transaction_pool::TransactionPool};
use reth_exex::ExExContext;
use reth_tracing::tracing::debug;

use crate::tracker::Tracker;

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
            Some(full_event) = all_events_stream.next() => tracker.handle_event(full_event),

            // Use chain notifications to track time to inclusion.
            Some(notification) = ctx.notifications.next() => {
                match notification {
                    Ok(notification) => ctx.events.send(tracker.handle_notification(notification))?,
                    Err(e) => {
                        debug!(target: "tracex", error = %e, "Notification error");
                        return Err(e);
                    }
                }
            }
        }
    }
}
