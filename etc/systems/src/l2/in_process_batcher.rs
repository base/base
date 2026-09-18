//! In-process batcher for L2 system test stacks.
//!
//! Runs `base-batcher-service` directly in the test process, eliminating the Docker
//! dependency for the batch submission layer. Mirrors the pattern used by
//! [`InProcessConsensus`](super::InProcessConsensus).

use std::time::Duration;

use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use base_batcher_encoder::DaType;
use base_batcher_service::{BatcherConfig, BatcherService};
use base_consensus_rpc::SyncStatusApiClient;
use base_protocol::BlockInfo;
use base_runtime::TokioRuntime;
use base_tx_manager::SignerConfig;
use eyre::Result;
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::{sync::watch, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

const INITIAL_SAFE_L2_HEAD_TIMEOUT: Duration = Duration::from_secs(60);
const INITIAL_SAFE_L2_HEAD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for starting an in-process batcher.
#[derive(Debug, Clone)]
pub struct InProcessBatcherConfig {
    /// L1 RPC endpoint for batch transaction submission.
    pub l1_rpc_url: Url,
    /// L2 execution RPC endpoint for reading L2 blocks.
    pub l2_rpc_url: Url,
    /// Rollup node RPC endpoint for fetching the rollup config.
    pub rollup_rpc_url: Url,
    /// Batcher private key for signing L1 transactions.
    pub batcher_key: B256,
    /// Whether to use short-lived calldata channels for deterministic tests.
    pub force_batch_submission: bool,
}

/// A running in-process batcher.
pub struct InProcessBatcher {
    cancellation: CancellationToken,
    failure_rx: watch::Receiver<Option<String>>,
    handle: JoinHandle<()>,
}

impl std::fmt::Debug for InProcessBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessBatcher").finish_non_exhaustive()
    }
}

impl InProcessBatcher {
    /// Starts an in-process batcher with the given configuration.
    pub async fn start(config: InProcessBatcherConfig) -> Result<Self> {
        Self::wait_for_initial_safe_l2_head(&config.rollup_rpc_url).await?;

        let signer = PrivateKeySigner::from_bytes(&config.batcher_key)
            .map_err(|e| eyre::eyre!("invalid batcher key: {e}"))?;
        let mut batcher_config = BatcherConfig {
            l1_rpc_url: vec![config.l1_rpc_url],
            l2_rpc_url: vec![config.l2_rpc_url],
            rollup_rpc_url: vec![config.rollup_rpc_url],
            signer: Some(SignerConfig::local(signer)),
            // SystemTestStack defaults come from the shared batcher config:
            // poll_interval: 1s, num_confirmations: 1, resubmission_timeout: 48s —
            // all set by BatcherConfig::default().
            ..BatcherConfig::default()
        };
        if config.force_batch_submission {
            batcher_config.encoder_config.da_type = DaType::Calldata;
        }
        let cancellation = CancellationToken::new();
        let runtime = TokioRuntime::with_token(cancellation.clone());
        let ready = BatcherService::new(batcher_config).setup(runtime).await?;
        let (failure_tx, failure_rx) = watch::channel(None);
        let handle = tokio::spawn(async move {
            if let Err(e) = ready.run().await {
                tracing::error!(error = %e, "in-process batcher exited with error");
                failure_tx.send_replace(Some(e.to_string()));
            }
        });
        Ok(Self { cancellation, failure_rx, handle })
    }

    /// Waits for the rollup node to initialize the safe L2 head used by batcher startup.
    ///
    /// The batcher reads this head during setup to anchor its channel timestamps. A consensus RPC
    /// endpoint can be reachable before initial derivation has populated the head, especially when
    /// several system-test stacks share an L1 fixture. Waiting here turns that startup race into a
    /// bounded, diagnosable wait rather than an immediate `safe L2 head is empty` failure.
    pub async fn wait_for_initial_safe_l2_head(rollup_rpc_url: &Url) -> Result<()> {
        let client =
            HttpClientBuilder::default().build(rollup_rpc_url.as_str()).map_err(|error| {
                eyre::eyre!("failed to build batcher readiness RPC client: {error}")
            })?;
        let mut last_status = None;
        let mut last_error = None;

        let wait = async {
            loop {
                match client.sync_status().await {
                    Ok(status) if status.safe_l2.block_info != BlockInfo::default() => {
                        info!(
                            safe_l2 = status.safe_l2.block_info.number,
                            unsafe_l2 = status.unsafe_l2.block_info.number,
                            current_l1 = status.current_l1.number,
                            "safe L2 head is ready for in-process batcher startup"
                        );
                        return;
                    }
                    Ok(status) => {
                        debug!(
                            safe_l2 = ?status.safe_l2,
                            unsafe_l2 = ?status.unsafe_l2,
                            current_l1 = ?status.current_l1,
                            "waiting for consensus to initialize safe L2 head before batcher startup"
                        );
                        last_status = Some(status);
                    }
                    Err(error) => {
                        warn!(
                            error = %error,
                            "optimism_syncStatus failed while waiting for batcher safe L2 head"
                        );
                        last_error = Some(error.to_string());
                    }
                }
                sleep(INITIAL_SAFE_L2_HEAD_POLL_INTERVAL).await;
            }
        };

        tokio::time::timeout(INITIAL_SAFE_L2_HEAD_TIMEOUT, wait)
            .await
            .map_err(|_| {
                eyre::eyre!(
                    "timed out after {:?} waiting for consensus to initialize the safe L2 head before \
                     batcher startup; last_sync_status={last_status:?}; last_sync_status_error={last_error:?}",
                    INITIAL_SAFE_L2_HEAD_TIMEOUT
                )
            })?;
        Ok(())
    }

    /// Returns the batcher failure if its service task has exited with an error.
    pub fn failure(&self) -> Option<String> {
        self.failure_rx.borrow().clone()
    }

    /// Stops batch submission while retaining the service handle for test-stack ownership.
    pub fn stop(&self) {
        self.cancellation.cancel();
    }

    /// Stops batch submission and waits for the service task to exit.
    pub async fn shutdown(mut self) -> Result<()> {
        let was_cancelled = self.cancellation.is_cancelled();
        let was_finished = self.handle.is_finished();
        self.cancellation.cancel();

        let join_result = match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut self.handle).await {
            Ok(result) => result,
            Err(_) => {
                return Err(eyre::eyre!("timed out waiting for in-process batcher to shut down"));
            }
        };
        if let Err(error) = join_result {
            return Err(eyre::eyre!("in-process batcher task failed: {error}"));
        }
        if let Some(error) = self.failure() {
            return Err(eyre::eyre!("in-process batcher service failed: {error}"));
        }
        if was_finished && !was_cancelled {
            return Err(eyre::eyre!("in-process batcher task exited unexpectedly"));
        }
        Ok(())
    }
}

impl Drop for InProcessBatcher {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, SyncStatus};
    use jsonrpsee::{RpcModule, server::ServerBuilder, types::ErrorObjectOwned};
    use tokio::{sync::watch, task::JoinHandle};
    use tokio_util::sync::CancellationToken;

    use super::InProcessBatcher;

    fn batcher(
        cancellation: CancellationToken,
        failure_rx: watch::Receiver<Option<String>>,
        handle: JoinHandle<()>,
    ) -> InProcessBatcher {
        InProcessBatcher { cancellation, failure_rx, handle }
    }

    #[derive(Clone)]
    struct SyncStatusServer {
        requests: Arc<AtomicUsize>,
        ready_status: SyncStatus,
    }

    #[tokio::test]
    async fn batcher_startup_waits_for_safe_l2_head() {
        let mut ready_status = SyncStatus::default();
        ready_status.safe_l2.block_info = BlockInfo::new(B256::repeat_byte(1), 1, B256::ZERO, 1);
        let server = SyncStatusServer { requests: Arc::new(AtomicUsize::new(0)), ready_status };
        let rpc_server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let address = rpc_server.local_addr().unwrap();
        let mut module = RpcModule::new(server.clone());
        module
            .register_async_method("optimism_syncStatus", |_, server, _| async move {
                let status = if server.requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    SyncStatus::default()
                } else {
                    server.ready_status.clone()
                };
                Ok::<_, ErrorObjectOwned>(status)
            })
            .unwrap();
        let handle = rpc_server.start(module);

        InProcessBatcher::wait_for_initial_safe_l2_head(
            &format!("http://{address}").parse().unwrap(),
        )
        .await
        .unwrap();

        handle.stop().unwrap();
        assert!(server.requests.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn shutdown_accepts_intentional_stop() {
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (_failure_tx, failure_rx) = watch::channel(None);
        let handle = tokio::spawn(async move { task_cancellation.cancelled().await });
        let batcher = batcher(cancellation, failure_rx, handle);

        batcher.stop();
        batcher.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_reports_service_failure() {
        let cancellation = CancellationToken::new();
        let (failure_tx, failure_rx) = watch::channel(None);
        let handle = tokio::spawn(async move {
            failure_tx.send_replace(Some("submission failed".to_owned()));
        });
        let batcher = batcher(cancellation, failure_rx, handle);
        tokio::task::yield_now().await;

        let error = batcher.shutdown().await.unwrap_err().to_string();
        assert!(error.contains("service failed: submission failed"), "{error}");
    }

    #[tokio::test]
    async fn shutdown_reports_task_panic() {
        let cancellation = CancellationToken::new();
        let (_failure_tx, failure_rx) = watch::channel(None);
        let handle = tokio::spawn(async { panic!("batcher panic") });
        let batcher = batcher(cancellation, failure_rx, handle);
        tokio::task::yield_now().await;

        let error = batcher.shutdown().await.unwrap_err().to_string();
        assert!(error.contains("task failed"), "{error}");
        assert!(error.contains("batcher panic"), "{error}");
    }

    #[tokio::test]
    async fn dropping_pending_shutdown_aborts_task() {
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }

        let cancellation = CancellationToken::new();
        let (_failure_tx, failure_rx) = watch::channel(None);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _guard = OnDrop(Some(dropped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        let batcher = batcher(cancellation, failure_rx, handle);

        let mut shutdown = Box::pin(batcher.shutdown());
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut shutdown).await.is_err());
        drop(shutdown);

        tokio::time::timeout(Duration::from_secs(1), dropped_rx).await.unwrap().unwrap();
    }
}
