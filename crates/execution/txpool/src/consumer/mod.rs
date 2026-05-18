//! Transaction pool consumer that drains pending transactions and broadcasts
//! them to downstream forwarders for delivery to block builders.

use std::sync::Arc;

use reth_tasks::TaskExecutor;
use reth_transaction_pool::{EthPoolTransaction, TransactionPool, ValidPoolTransaction};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

mod config;
pub use config::ConsumerConfig;

mod metrics;
pub use metrics::Metrics as ConsumerMetrics;

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
    P::Transaction: EthPoolTransaction + Clone,
{
    /// Creates and spawns a [`Consumer`] as a blocking task on the executor.
    pub fn spawn(pool: P, config: ConsumerConfig, executor: &TaskExecutor) -> Self {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        let broadcast_sender = sender.clone();
        let cancel = CancellationToken::new();
        let mut consumer = Consumer::new(pool, config, broadcast_sender, cancel.child_token());

        executor.spawn_blocking_task(Box::pin(async move {
            consumer.run();
        }));

        Self { sender, cancel }
    }

    /// Cancels the consumer loop.
    /// The consumer is checking for cancellation extremely often, so we don't need to have
    /// a "long" timeout for it as it will shutdown within a few milliseconds anyway
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
