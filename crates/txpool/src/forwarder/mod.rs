use std::sync::Arc;

use jsonrpsee::http_client::HttpClientBuilder;
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

mod config;
pub use config::ForwarderConfig;

mod metrics;
pub use metrics::ForwarderMetrics;

mod task;
pub use task::{Forwarder, ValidTransaction};

/// Handle for the set of forwarder tasks (one per builder URL).
///
/// Cancels all forwarder tasks on drop.
pub struct ForwarderHandle {
    cancel: CancellationToken,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ForwarderHandle {
    /// Spawns one forwarder async task per builder URL, each subscribing to
    /// the given broadcast sender.
    pub fn spawn<T>(
        sender: &broadcast::Sender<Arc<ValidPoolTransaction<T>>>,
        config: ForwarderConfig,
    ) -> Self
    where
        T: PoolTransaction + 'static,
        <T as PoolTransaction>::Consensus: alloy_eips::Encodable2718,
    {
        let cancel = CancellationToken::new();
        let mut tasks = Vec::with_capacity(config.builder_urls.len());

        for url in &config.builder_urls {
            let client = match HttpClientBuilder::default().build(url) {
                Ok(client) => client,
                Err(err) => {
                    error!(
                        builder_url = %url,
                        error = %err,
                        "failed to build HTTP client for forwarder, skipping",
                    );
                    continue;
                }
            };

            let receiver = sender.subscribe();
            let metrics = ForwarderMetrics::default();
            let forwarder = Forwarder::new(
                url.clone(),
                client,
                receiver,
                config.clone(),
                metrics,
                cancel.child_token(),
            );

            let handle = tokio::spawn(async move {
                forwarder.run().await;
            });

            info!(builder_url = %url, "spawned transaction forwarder");
            tasks.push(handle);
        }

        Self { cancel, _tasks: tasks }
    }
}

impl Drop for ForwarderHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl std::fmt::Debug for ForwarderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwarderHandle")
            .field("tasks", &self._tasks.len())
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}
