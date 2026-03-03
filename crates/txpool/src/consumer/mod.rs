mod config;
pub use config::ConsumerConfig;

mod metrics;
pub use metrics::ConsumerMetrics;

mod validator;
pub use validator::RecentlySent;

mod task;
use std::sync::Arc;

use reth_transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
pub use task::Consumer;
use tokio::sync::broadcast;

/// Handle returned by [`ConsumerHandle::spawn`].
///
/// Holds the broadcast sender so that downstream forwarders (one per builder)
/// can each call [`.subscribe()`](broadcast::Sender::subscribe) to receive
/// every deduplicated transaction independently. Slow receivers that fall
/// behind will receive [`broadcast::error::RecvError::Lagged`], naturally
/// shedding stale transactions.
#[derive(Debug)]
pub struct ConsumerHandle<T: PoolTransaction> {
    /// Broadcast sender — call `.subscribe()` to create a new receiver.
    pub sender: broadcast::Sender<Arc<ValidPoolTransaction<T>>>,
}

impl<T: PoolTransaction> ConsumerHandle<T> {
    /// Spawns the consumer on a dedicated blocking thread and returns a
    /// handle for subscribing forwarders.
    ///
    /// The consumer continuously refreshes the pool's `best_transactions()`
    /// iterator, deduplicates by hash, and broadcasts transactions. It shuts
    /// down when all receivers are dropped.
    pub fn spawn<P>(pool: P, config: ConsumerConfig) -> Self
    where
        P: TransactionPool<Transaction = T> + Send + 'static,
    {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        let broadcast_sender = sender.clone();
        let metrics = ConsumerMetrics::default();
        let consumer = Consumer::new(pool, config, broadcast_sender, metrics);

        tokio::task::spawn_blocking(move || {
            consumer.run();
        });

        Self { sender }
    }
}
