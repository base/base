use std::{sync::Arc, time::Duration};

use jsonrpsee::http_client::HttpClientBuilder;
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

mod config;
pub use config::ForwarderConfig;

mod metrics;
pub use metrics::ForwarderMetrics;

mod rpc;
pub use rpc::{BuilderApiClient, BuilderApiServer};

mod task;
pub use task::{Forwarder, ValidTransaction};

/// Handle for the set of forwarder tasks (one per builder URL).
///
/// Cancels all forwarder tasks on drop. For a clean shutdown that waits for
/// in-flight RPC requests to complete, use [`ForwarderHandle::shutdown`].
pub struct ForwarderHandle {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
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
        let config = Arc::new(config);

        for url in &config.builder_urls {
            let client = match HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(5))
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
            let metrics = ForwarderMetrics::new(url.as_str());
            let forwarder = Forwarder::new(
                url.clone(),
                client,
                receiver,
                Arc::clone(&config),
                metrics,
                cancel.child_token(),
            );

            let handle = tokio::spawn(async move {
                forwarder.run().await;
            });

            info!(builder_url = %url, "spawned transaction forwarder");
            tasks.push(handle);
        }

        if tasks.is_empty() {
            warn!("no forwarder tasks spawned — check builder_urls config");
        }

        Self { cancel, tasks }
    }
}

impl ForwarderHandle {
    /// Gracefully shuts down all forwarder tasks.
    ///
    /// Signals cancellation and waits up to 30 seconds for each forwarder to
    /// drain its buffer and complete in-flight RPC requests.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();

        let tasks = std::mem::take(&mut self.tasks);
        if tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(tasks))
            .await
            .is_err()
        {
            warn!("forwarder tasks did not finish within shutdown timeout");
        }
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
            .field("tasks", &self.tasks.len())
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}
