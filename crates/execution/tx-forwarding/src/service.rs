//! Transaction forwarding service lifecycle.

use std::{sync::Arc, time::Duration};

use alloy_eips::Encodable2718;
use base_execution_txpool::{BundleTransaction, NoExtensions, ValidatedTransactionExtensions};
use futures::{StreamExt, future::join_all, stream::FuturesUnordered};
use jsonrpsee::http_client::HttpClientBuilder;
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use url::Url;

use crate::{
    TxForwardingConfig,
    forwarder::{DestinationForwarder, ForwardRequest},
    reader::DestinationReader,
};

/// Why a forwarding destination could not be started.
#[derive(Debug, thiserror::Error)]
pub enum ForwardingSetupError {
    /// A destination endpoint could not be turned into an RPC client.
    #[error("cannot build an RPC client for destination `{url}`: {source}")]
    Client {
        /// The offending endpoint.
        url: Url,
        /// The underlying client-construction failure.
        ///
        /// Boxed to keep the `Err` variant small: the success path is the common one, and an
        /// unboxed client error would widen every `Result` this returns to its size.
        #[source]
        source: Box<jsonrpsee::core::client::Error>,
    },
}

/// Default maximum time allowed for destination queues and in-flight requests to drain.
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns transaction forwarding configuration and starts destination pipelines.
#[derive(Debug)]
pub struct TxForwardingService {
    config: TxForwardingConfig,
}

impl TxForwardingService {
    /// Creates a transaction forwarding service.
    pub const fn new(config: TxForwardingConfig) -> Self {
        Self { config }
    }

    /// Starts one independently deduplicated forwarding pipeline per destination.
    pub fn spawn<P>(self, pool: P, executor: &TaskExecutor) -> TxForwardingHandle
    where
        P: TransactionPool + Clone + Send + 'static,
        P::Transaction: PoolTransaction + BundleTransaction,
        <P::Transaction as PoolTransaction>::Consensus: Encodable2718,
    {
        self.spawn_with_extensions::<P, NoExtensions>(pool, executor)
    }

    /// Starts forwarding with an extension payload on the transaction wire format.
    pub fn spawn_with_extensions<P, E>(self, pool: P, executor: &TaskExecutor) -> TxForwardingHandle
    where
        P: TransactionPool + Clone + Send + 'static,
        P::Transaction: PoolTransaction + BundleTransaction,
        <P::Transaction as PoolTransaction>::Consensus: Encodable2718,
        E: ValidatedTransactionExtensions<P::Transaction>,
    {
        let reader_cancel = CancellationToken::new();
        if !self.config.enabled {
            return TxForwardingHandle {
                reader_cancel,
                reader_tasks: Vec::new(),
                forwarder_tasks: Vec::new(),
                shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            };
        }

        let reader_config = self.config.reader_config();
        let forwarder_config = Arc::new(self.config.forwarder_config());
        let mut reader_tasks = Vec::with_capacity(self.config.builder_urls.len());
        let mut forwarder_tasks = Vec::with_capacity(self.config.builder_urls.len());

        for builder_url in &self.config.builder_urls {
            let client = match HttpClientBuilder::default()
                .request_timeout(forwarder_config.request_timeout)
                .build(builder_url.as_str())
            {
                Ok(client) => client,
                Err(err) => {
                    error!(
                        builder_url = %builder_url,
                        error = %err,
                        "failed to build transaction forwarding client",
                    );
                    continue;
                }
            };

            let (sender, receiver) = mpsc::channel(reader_config.channel_capacity);
            // `E` is named explicitly: nothing else in this expression pins the queue's item type,
            // since both ends are generic over it.
            let mut reader = DestinationReader::<P, E>::new(
                pool.clone(),
                reader_config.clone(),
                sender,
                reader_cancel.child_token(),
                builder_url.clone(),
            );
            let forwarder = DestinationForwarder::new(
                builder_url.clone(),
                client,
                receiver,
                Arc::clone(&forwarder_config),
            );

            reader_tasks.push(executor.spawn_blocking_task(Box::pin(async move {
                reader.run();
            })));
            forwarder_tasks.push(executor.spawn_task(Box::pin(async move {
                forwarder.run().await;
            })));
            info!(builder_url = %builder_url, "started transaction forwarding destination");
        }

        TxForwardingHandle {
            reader_cancel,
            reader_tasks,
            forwarder_tasks,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }

    /// Starts one forwarder per destination, driven by queues the caller owns.
    ///
    /// The pool-polling reader is bypassed entirely: the caller produces requests itself and
    /// holds the sending halves. Use this when requests arrive by push rather than by draining a
    /// [`TransactionPool`], while still getting this crate's batching, rate limiting, retries,
    /// metrics and shutdown. The caller chooses its queue capacity and its own overflow policy,
    /// which is the point — a producer holding a lock needs to drop rather than wait, and only it
    /// knows which messages may be dropped independently.
    ///
    /// Requests reach a destination in queue order, so a producer may rely on submission order.
    ///
    /// Unlike [`Self::spawn`], an unusable endpoint is an error rather than a logged skip: a caller
    /// that asked for N destinations and silently got N-1 has a delivery hole it cannot observe.
    pub fn spawn_requests<R: ForwardRequest>(
        &self,
        destinations: Vec<(Url, mpsc::Receiver<R>)>,
        executor: &TaskExecutor,
    ) -> Result<TxForwardingHandle, ForwardingSetupError> {
        let forwarder_config = Arc::new(self.config.forwarder_config());
        let mut forwarder_tasks = Vec::with_capacity(destinations.len());

        for (url, receiver) in destinations {
            let client = HttpClientBuilder::default()
                .request_timeout(forwarder_config.request_timeout)
                .build(url.as_str())
                .map_err(|source| ForwardingSetupError::Client {
                    url: url.clone(),
                    source: Box::new(source),
                })?;

            let forwarder = DestinationForwarder::new(
                url.clone(),
                client,
                receiver,
                Arc::clone(&forwarder_config),
            );
            forwarder_tasks.push(executor.spawn_task(Box::pin(async move {
                forwarder.run().await;
            })));
            info!(destination = %url, "started request forwarding destination");
        }

        Ok(TxForwardingHandle {
            reader_cancel: CancellationToken::new(),
            reader_tasks: Vec::new(),
            forwarder_tasks,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        })
    }
}

/// Handle used to gracefully stop a [`TxForwardingService`].
pub struct TxForwardingHandle {
    reader_cancel: CancellationToken,
    reader_tasks: Vec<JoinHandle<()>>,
    forwarder_tasks: Vec<JoinHandle<()>>,
    shutdown_timeout: Duration,
}

impl TxForwardingHandle {
    /// Overrides how long [`Self::shutdown`] lets forwarders drain before aborting them.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Stops pool readers, drains each destination queue, and reports task outcomes.
    pub async fn shutdown(self) -> ShutdownReport {
        self.reader_cancel.cancel();

        let reader_results = join_all(self.reader_tasks).await;
        let readers_completed = reader_results.iter().filter(|result| result.is_ok()).count();
        let mut task_failures = reader_results.len() - readers_completed;

        let mut forwarders: FuturesUnordered<_> = self.forwarder_tasks.into_iter().collect();
        let deadline = tokio::time::Instant::now() + self.shutdown_timeout;
        let mut forwarders_completed = 0;
        let mut timed_out = false;

        while !forwarders.is_empty() {
            match tokio::time::timeout_at(deadline, forwarders.next()).await {
                Ok(Some(Ok(()))) => forwarders_completed += 1,
                Ok(Some(Err(_))) => task_failures += 1,
                Ok(None) => break,
                Err(_) => {
                    timed_out = true;
                    warn!("transaction forwarding tasks did not finish within shutdown timeout");
                    for task in &forwarders {
                        task.abort();
                    }
                    while let Some(result) = forwarders.next().await {
                        match result {
                            Ok(()) => forwarders_completed += 1,
                            Err(_) => task_failures += 1,
                        }
                    }
                }
            }
        }

        ShutdownReport { readers_completed, forwarders_completed, task_failures, timed_out }
    }
}

impl std::fmt::Debug for TxForwardingHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxForwardingHandle")
            .field("readers", &self.reader_tasks.len())
            .field("forwarders", &self.forwarder_tasks.len())
            .field("cancelled", &self.reader_cancel.is_cancelled())
            .finish()
    }
}

/// Summary of transaction forwarding task shutdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Reader tasks that stopped cleanly.
    pub readers_completed: usize,
    /// Forwarder tasks that drained and stopped cleanly.
    pub forwarders_completed: usize,
    /// Tasks that exited with a join error.
    pub task_failures: usize,
    /// Whether draining forwarders exceeded the shutdown timeout.
    pub timed_out: bool,
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Mutex, time::Duration};

    use alloy_primitives::{Address, B256, Bytes};
    use base_execution_txpool::ValidatedTransaction;
    use jsonrpsee::{RpcModule, server::Server};
    use reth_tasks::{RuntimeBuilder, RuntimeConfig, TokioConfig};
    use serde_json::Value;
    use tokio::sync::oneshot;

    use super::*;
    use crate::InsertValidatedTransaction;

    /// A [`TaskExecutor`] attached to the test's own tokio runtime, so spawned forwarders share it
    /// rather than standing up a second one per test.
    fn test_runtime() -> TaskExecutor {
        RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
        )
        .build()
        .expect("test runtime builds")
    }

    fn insert(byte: u8) -> InsertValidatedTransaction {
        InsertValidatedTransaction {
            transaction: ValidatedTransaction {
                sender: Address::repeat_byte(byte),
                raw: Bytes::from(vec![byte]),
                min_block_number: None,
                max_block_number: None,
                min_timestamp: None,
                max_timestamp: None,
                extensions: Default::default(),
            },
            tx_hash: B256::repeat_byte(byte),
        }
    }

    /// An endpoint a caller asked for but that cannot be reached must fail startup rather than be
    /// skipped, so a delivery hole is never silent. `Url` parses a `ws` scheme happily; the HTTP
    /// client is what rejects it.
    #[tokio::test]
    async fn spawn_requests_rejects_an_unusable_endpoint() {
        let runtime = test_runtime();
        let (_sender, receiver) = mpsc::channel::<InsertValidatedTransaction>(1);
        let url: Url = "ws://destination.invalid".parse().expect("parses as a url");

        let error = TxForwardingService::new(TxForwardingConfig::new(vec![url.clone()]))
            .spawn_requests(vec![(url.clone(), receiver)], &runtime)
            .expect_err("an unusable endpoint must fail startup");

        let ForwardingSetupError::Client { url: reported, .. } = error;
        assert_eq!(reported, url, "the error must name the offending endpoint");
    }

    /// Requests a caller pushes onto its own queue reach the destination, and shutdown drains what
    /// is still queued rather than dropping it.
    #[tokio::test]
    async fn spawn_requests_delivers_from_a_caller_owned_queue() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut module = RpcModule::new(Arc::clone(&received));
        module
            .register_method("base_insertValidatedTransaction", |params, received, _| {
                let (transaction,): (Value,) = params.parse()?;
                received.lock().unwrap().push(transaction);
                Ok::<_, jsonrpsee::types::ErrorObjectOwned>(())
            })
            .unwrap();
        let server = Server::builder().build(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let url: Url = format!("http://{}", server.local_addr().unwrap()).parse().unwrap();
        let _handle = server.start(module);

        let runtime = test_runtime();
        let (sender, receiver) = mpsc::channel(4);
        sender.send(insert(1)).await.unwrap();
        sender.send(insert(2)).await.unwrap();
        drop(sender);

        let forwarding = TxForwardingService::new(TxForwardingConfig::new(vec![url.clone()]))
            .spawn_requests(vec![(url, receiver)], &runtime)
            .expect("a reachable endpoint starts");

        let report = tokio::time::timeout(Duration::from_secs(5), forwarding.shutdown())
            .await
            .expect("shutdown must not hang");

        assert_eq!(report.forwarders_completed, 1);
        assert_eq!(report.task_failures, 0);
        assert!(!report.timed_out);
        assert_eq!(received.lock().unwrap().len(), 2, "both queued requests must be delivered");
    }

    #[tokio::test]
    async fn shutdown_cancels_readers_before_waiting_for_forwarders() {
        let reader_cancel = CancellationToken::new();
        let reader_signal = reader_cancel.child_token();
        let (reader_stopped, wait_for_reader) = oneshot::channel();
        let reader_task = tokio::spawn(async move {
            reader_signal.cancelled().await;
            reader_stopped.send(()).unwrap();
        });
        let forwarder_task = tokio::spawn(async move {
            wait_for_reader.await.unwrap();
        });
        let handle = TxForwardingHandle {
            reader_cancel,
            reader_tasks: vec![reader_task],
            forwarder_tasks: vec![forwarder_task],
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        };

        let report = handle.shutdown().await;

        assert_eq!(report.readers_completed, 1);
        assert_eq!(report.forwarders_completed, 1);
        assert_eq!(report.task_failures, 0);
        assert!(!report.timed_out);
    }

    #[tokio::test]
    async fn shutdown_reports_join_failures() {
        let handle = TxForwardingHandle {
            reader_cancel: CancellationToken::new(),
            reader_tasks: vec![tokio::spawn(async { panic!("reader failed") })],
            forwarder_tasks: vec![tokio::spawn(async { panic!("forwarder failed") })],
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        };

        let report = handle.shutdown().await;

        assert_eq!(report.readers_completed, 0);
        assert_eq!(report.forwarders_completed, 0);
        assert_eq!(report.task_failures, 2);
        assert!(!report.timed_out);
    }

    #[tokio::test]
    async fn shutdown_aborts_forwarders_after_timeout() {
        let handle = TxForwardingHandle {
            reader_cancel: CancellationToken::new(),
            reader_tasks: Vec::new(),
            forwarder_tasks: vec![tokio::spawn(std::future::pending())],
            shutdown_timeout: Duration::from_millis(50),
        };

        let report = handle.shutdown().await;

        assert_eq!(report.readers_completed, 0);
        assert_eq!(report.forwarders_completed, 0);
        assert_eq!(report.task_failures, 1);
        assert!(report.timed_out);
    }
}
