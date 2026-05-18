//! Transaction forwarder that relays pool transactions to a remote RPC endpoint.

use std::{sync::Arc, time::Duration};

use jsonrpsee::http_client::HttpClientBuilder;
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::transaction::{BundleTransaction, OpPooledTx};

mod config;
pub use config::ForwarderConfig;

mod metrics;
pub use metrics::ForwarderMetrics;

mod task;
pub use task::Forwarder;

/// Set of spawned forwarder tasks (one per builder URL).
///
/// Exposes the [`CancellationToken`] and task join handles so callers can
/// wire up graceful shutdown (e.g. via reth's
/// `spawn_with_graceful_shutdown_signal`).
pub struct SpawnedForwarder {
    /// Cancellation token — cancel this to stop all forwarder loops.
    pub cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl SpawnedForwarder {
    /// Spawns one forwarder async task per builder URL, each subscribing to
    /// the given broadcast sender.
    pub fn spawn<T>(
        sender: &broadcast::Sender<Arc<ValidPoolTransaction<T>>>,
        config: ForwarderConfig,
        executor: &TaskExecutor,
    ) -> Self
    where
        T: PoolTransaction + BundleTransaction + OpPooledTx + 'static,
        <T as PoolTransaction>::Consensus: alloy_eips::Encodable2718,
    {
        let cancel = CancellationToken::new();
        let mut tasks = Vec::with_capacity(config.builder_urls.len());
        let config = Arc::new(config);

        for url in &config.builder_urls {
            let client = match HttpClientBuilder::default()
                .request_timeout(config.request_timeout)
                .build(url.as_str())
            {
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
            let forwarder = Forwarder::new(
                url.clone(),
                client,
                receiver,
                Arc::clone(&config),
                cancel.child_token(),
            );

            let handle = executor.spawn_task(Box::pin(async move {
                forwarder.run().await;
            }));

            info!(builder_url = %url, "spawned transaction forwarder");
            tasks.push(handle);
        }

        Self { cancel, tasks }
    }

    /// Cancels all forwarder tasks and waits up to 30 seconds for each to
    /// drain its buffer and complete in-flight RPC requests.
    pub async fn shutdown(self) {
        self.cancel.cancel();

        if tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(self.tasks))
            .await
            .is_err()
        {
            warn!("forwarder tasks did not finish within shutdown timeout");
        }
    }
}

impl std::fmt::Debug for SpawnedForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedForwarder")
            .field("tasks", &self.tasks.len())
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}
