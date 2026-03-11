use std::sync::Arc;

use reth_tasks::TaskExecutor;
use reth_transaction_pool::{TransactionPool, ValidPoolTransaction};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

mod config;
pub use config::ConsumerConfig;

mod metrics;
pub use metrics::ConsumerMetrics;

mod validator;
pub use validator::RecentlySent;

mod task;
pub use task::Consumer;

/// Result of spawning a [`Consumer`] via the reth task executor.
///
/// Holds the broadcast sender so that downstream forwarders (one per builder)
/// can each call [`.subscribe()`](broadcast::Sender::subscribe) to receive
/// every deduplicated transaction independently.
pub struct SpawnedConsumer<P: TransactionPool> {
    /// Broadcast sender — call `.subscribe()` to create a new receiver.
    pub sender: broadcast::Sender<Arc<ValidPoolTransaction<P::Transaction>>>,
    /// Cancellation token — cancel this to stop the consumer loop.
    pub cancel: CancellationToken,
}

impl<P> SpawnedConsumer<P>
where
    P: TransactionPool + Send + 'static,
{
    /// Creates and spawns a [`Consumer`] as a blocking task on the executor.
    pub fn spawn(pool: P, config: ConsumerConfig, executor: &TaskExecutor) -> Self {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        let broadcast_sender = sender.clone();
        let metrics = ConsumerMetrics::default();
        let cancel = CancellationToken::new();
        let mut consumer =
            Consumer::new(pool, config, broadcast_sender, metrics, cancel.child_token());

        executor.spawn_blocking_task(Box::pin(async move {
            consumer.run();
        }));

        Self { sender, cancel }
    }

    /// Cancels the consumer loop.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

impl<P: TransactionPool> std::fmt::Debug for SpawnedConsumer<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedConsumer")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}
