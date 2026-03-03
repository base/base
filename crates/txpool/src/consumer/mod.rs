use std::{sync::Arc, thread};

use reth_tasks::spawn_os_thread;
use reth_transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
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

/// Handle returned by [`ConsumerHandle::spawn`].
///
/// Holds the broadcast sender so that downstream forwarders (one per builder)
/// can each call [`.subscribe()`](broadcast::Sender::subscribe) to receive
/// every deduplicated transaction independently. Cancels the background
/// consumer on drop.
pub struct ConsumerHandle<T: PoolTransaction> {
    /// Broadcast sender — call `.subscribe()` to create a new receiver.
    pub sender: broadcast::Sender<Arc<ValidPoolTransaction<T>>>,
    cancel: CancellationToken,
    handle: Option<thread::JoinHandle<()>>,
}

impl<T: PoolTransaction> ConsumerHandle<T> {
    /// Spawns the consumer on a dedicated OS thread and returns a handle for
    /// subscribing forwarders.
    pub fn spawn<P>(pool: P, config: ConsumerConfig) -> Self
    where
        P: TransactionPool<Transaction = T> + Send + 'static,
    {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        let broadcast_sender = sender.clone();
        let metrics = ConsumerMetrics::default();
        let cancel = CancellationToken::new();
        let consumer = Consumer::new(pool, config, broadcast_sender, metrics, cancel.child_token());

        let handle = spawn_os_thread("txpool-consumer", move || {
            consumer.run();
        });

        Self { sender, cancel, handle: Some(handle) }
    }
}

impl<T: PoolTransaction> Drop for ConsumerHandle<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl<T: PoolTransaction> std::fmt::Debug for ConsumerHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsumerHandle")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}
