//! Batcher service startup and wiring.

use std::{
    future::{Future, pending},
    sync::Arc,
    task::Poll,
    time::Duration,
};

use alloy_provider::{Provider, ProviderBuilder, ProviderLayer, RootProvider};
use backon::Retryable;
use base_balance_monitor::BalanceMonitorLayer;
use base_batcher_admin::AdminServer;
use base_batcher_core::{
    AdminHandle, BatchDriver, BatchDriverInputs, DaThrottle, ThrottleController, ThrottleStrategy,
};
use base_batcher_encoder::{BatchEncoder, BatcherMetrics};
use base_batcher_source::{HybridL1HeadSource, PollingBlockSource};
use base_common_network::Base;
use base_consensus_rpc::RollupNodeApiClient;
use base_protocol::BlockInfo;
use base_retry::{DEFAULT_UNBOUNDED_MAX_DELAY, RetryConfig};
use base_runtime::TokioRuntime;
use base_tx_manager::{BaseTxMetrics, SimpleTxManager};
use futures::{
    FutureExt, StreamExt, TryFutureExt,
    future::BoxFuture,
    stream::{self, BoxStream, FuturesUnordered},
};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    BatcherConfig, DerivationStatusPoller, DerivationStatusProvider, L2BlockParityMonitor,
    L2BlockParityMonitorConfig, MAX_CHECK_RECENT_TXS_DEPTH, RecentTxSyncTarget,
    RpcL1HeadPollingSource, RpcL2BlockProvider, RpcPollingSource, SystemConfigBatcher,
    ThrottlePusher,
};

const WEI_PER_ETHER: f64 = 1_000_000_000_000_000_000.0;

/// Concrete driver type produced by [`BatcherService::setup`].
///
/// Private — callers interact only through [`ReadyBatcher`].
type ServiceDriver = BatchDriver<
    TokioRuntime,
    BatchEncoder,
    PollingBlockSource<RpcPollingSource, TokioRuntime>,
    SimpleTxManager<RootProvider>,
    HybridL1HeadSource<RpcL1HeadPollingSource>,
>;

/// A fully-initialised batcher ready to run the submission loop.
///
/// Created by [`BatcherService::setup`]. All connections are live and the
/// rollup config has been fetched. Call [`run`](Self::run) to enter the
/// main driver loop, or spawn it in a background task for in-process use.
#[derive(derive_more::Debug)]
pub struct ReadyBatcher {
    #[debug(skip)]
    driver: ServiceDriver,
    #[debug(skip)]
    admin_server: Option<AdminServer>,
    #[debug(skip)]
    background_tasks: Vec<(&'static str, BoxFuture<'static, eyre::Result<()>>)>,
    #[debug(skip)]
    cancellation: CancellationToken,
}

impl ReadyBatcher {
    /// Run the batch submission loop until the runtime is cancelled.
    pub async fn run(self) -> eyre::Result<()> {
        info!("batcher driver running");
        let Self { driver, admin_server, background_tasks, cancellation } = self;
        let background_cancellation = cancellation.clone();
        let background_task_exit = async move {
            let mut background_tasks = background_tasks
                .into_iter()
                .map(|(task_name, task)| async move { (task_name, task.await) })
                .collect::<FuturesUnordered<_>>();
            tokio::select! {
                biased;
                () = background_cancellation.cancelled() => {}
                Some((task_name, result)) = background_tasks.next(), if !background_tasks.is_empty() => {
                    match result {
                        Ok(()) => eyre::bail!("{task_name} exited unexpectedly"),
                        Err(error) => eyre::bail!("{task_name} failed: {error}"),
                    }
                }
            }

            while let Some((task_name, result)) = background_tasks.next().await {
                if let Err(error) = result {
                    warn!(
                        task = task_name,
                        error = %error,
                        "background task failed during shutdown"
                    );
                }
            }

            Ok::<_, eyre::Report>(())
        };
        tokio::pin!(background_task_exit);
        let driver_run = driver.run();
        tokio::pin!(driver_run);
        let admin_stopped = async {
            match admin_server.as_ref() {
                Some(admin) => admin.stopped().await,
                None => pending().await,
            }
        };
        tokio::pin!(admin_stopped);
        let mut admin_active = admin_server.is_some();

        loop {
            tokio::select! {
                r = &mut driver_run => {
                    cancellation.cancel();
                    let driver_result = r;
                    let background_result = background_task_exit.as_mut().await;
                    driver_result?;
                    background_result?;
                    break;
                }
                r = &mut background_task_exit => {
                    cancellation.cancel();
                    r?;
                    driver_run.await?;
                    break;
                }
                () = &mut admin_stopped, if admin_active => {
                    admin_active = false;
                    warn!("admin server stopped unexpectedly; batcher continues without admin API");
                }
            }
        }
        info!("batcher service shutting down");
        Ok(())
    }
}

/// The batcher service.
///
/// Wires the encoder, block source, L1 head source, transaction manager, and driver.
/// Call [`setup`](Self::setup) to initialise all components, then call
/// [`ReadyBatcher::run`] to enter the submission loop.
#[derive(Debug)]
pub struct BatcherService {
    /// Full batcher configuration.
    config: BatcherConfig,
}

impl BatcherService {
    /// Create a new [`BatcherService`] from the given configuration.
    pub const fn new(config: BatcherConfig) -> Self {
        Self { config }
    }

    /// Build the live L1 head stream for the given optional L1 WebSocket URL.
    ///
    /// When `url` is `Some`, connects a dedicated WS provider, subscribes to new L1
    /// block headers and streams their block numbers. The stream owns the provider, so
    /// the connection lives as long as the stream does.
    ///
    /// When `url` is `None`, or if connecting or subscribing fails, returns a stream that
    /// never yields so that [`HybridL1HeadSource`] relies on polling alone.
    ///
    /// `l1_head_subscription_active` is 1 while the subscription streams heads, and 0 once
    /// the batcher relies on polling alone.
    async fn build_l1_head_stream(url: Option<&Url>) -> BoxStream<'static, u64> {
        let active = BatcherMetrics::l1_head_subscription_active();
        active.set(0.0);

        let Some(url) = url else {
            return stream::pending().boxed();
        };

        let ws_provider = match ProviderBuilder::new().connect(url.as_str()).await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, l1_ws = %url, "failed to connect L1 WS provider; falling back to polling");
                return stream::pending().boxed();
            }
        };

        let sub = match ws_provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to subscribe to new L1 blocks; falling back to polling");
                return stream::pending().boxed();
            }
        };

        active.set(1.0);
        sub.into_stream()
            .map(move |header| {
                // Capture the provider: dropping it closes the connection and ends the stream.
                let _keep_alive = &ws_provider;
                header.number
            })
            // Mark the subscription down once alloy gives up reconnecting and the stream ends.
            .chain(stream::poll_fn(move |_| {
                active.set(0.0);
                Poll::Ready(None)
            }))
            .boxed()
    }

    /// Retry a one-shot startup RPC until it succeeds or `timeout` elapses.
    ///
    /// Uses [`RetryConfig`] for exponential backoff with jitter.
    async fn rpc_retry<T, E, F, Fut>(
        op: &'static str,
        retry: RetryConfig,
        timeout: Duration,
        f: F,
    ) -> eyre::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let attempt = f.retry(retry.to_backoff_builder()).notify(|error, delay| {
            warn!(
                error = %error,
                op,
                backoff_ms = delay.as_millis(),
                "startup RPC failed, backing off"
            );
        });
        match tokio::time::timeout(timeout, attempt).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(eyre::eyre!("{op} failed: {error}")),
            Err(_) => Err(eyre::eyre!("{op} timed out after {timeout:?}")),
        }
    }

    /// Block until the rollup node has processed `target_l1`, or until `timeout` elapses.
    ///
    /// RPC errors use exponential backoff capped at [`DEFAULT_UNBOUNDED_MAX_DELAY`].
    async fn wait_for_node_sync(
        rollup_client: &HttpClient,
        target_l1: u64,
        poll_interval: Duration,
        timeout: Duration,
    ) -> eyre::Result<()> {
        info!(
            target_l1 = %target_l1,
            timeout_secs = %timeout.as_secs(),
            "waiting for rollup node to process L1 target"
        );
        let wait = async {
            let mut error_backoff = poll_interval;
            loop {
                match rollup_client.sync_status().await {
                    Ok(status) if status.current_l1.number >= target_l1 => {
                        info!(
                            current_l1 = %status.current_l1.number,
                            unsafe_l2 = %status.unsafe_l2.block_info.number,
                            local_safe_l2 = %status.local_safe_l2.block_info.number,
                            "rollup node reports sync, proceeding with batcher startup"
                        );
                        return;
                    }
                    Ok(status) => {
                        error_backoff = poll_interval;
                        info!(
                            target_l1 = %target_l1,
                            current_l1 = %status.current_l1.number,
                            "rollup node not yet synced, waiting"
                        );
                        tokio::time::sleep(poll_interval).await;
                    }
                    Err(error) => {
                        warn!(
                            error = %error,
                            backoff_ms = error_backoff.as_millis(),
                            "optimism_syncStatus RPC failed during wait, backing off"
                        );
                        tokio::time::sleep(error_backoff).await;
                        error_backoff = (error_backoff * 2).min(DEFAULT_UNBOUNDED_MAX_DELAY);
                    }
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| eyre::eyre!("wait_for_node_sync timed out"))
    }

    /// Initialise all batcher components and return a [`ReadyBatcher`].
    ///
    /// Requires a signer, connects to the L2 RPC, fetches the rollup config, whose batch inbox
    /// the batcher posts to and must be the shadow inbox in shadow mode, connects to L1, checks
    /// outside shadow mode that the signer is the batcher the L1 `SystemConfig` authorizes, and
    /// constructs the driver. One-shot startup RPCs retry with exponential backoff until
    /// [`BatcherConfig::wait_node_sync_timeout`]. Returns an error if any of those steps fail,
    /// before any background work is spawned.
    ///
    /// The runtime's cancellation token is forwarded to the derivation-status poller
    /// spawned here so it stops cleanly when the batcher shuts down.
    pub async fn setup(self, runtime: TokioRuntime) -> eyre::Result<ReadyBatcher> {
        let cancellation = runtime.token().clone();
        let mut background_tasks = Vec::new();
        self.config.encoder_config.validate()?;
        if let Some(throttle) = &self.config.throttle {
            throttle.validate()?;
        }

        if self.config.poll_interval.is_zero() {
            eyre::bail!("poll_interval must be greater than zero");
        }
        if self.config.max_pending_transactions == 0 {
            eyre::bail!(
                "max_pending_transactions must be greater than zero: the batcher would never \
                 submit a transaction"
            );
        }
        if self.config.stopped && self.config.admin_addr.is_none() {
            eyre::bail!(
                "--stopped requires --admin-port: the batcher would start stopped with no way to \
                 start it because the admin JSON-RPC server is not enabled"
            );
        }
        if self.config.check_recent_txs_depth > MAX_CHECK_RECENT_TXS_DEPTH {
            eyre::bail!(
                "check_recent_txs_depth {} exceeds maximum of {}",
                self.config.check_recent_txs_depth,
                MAX_CHECK_RECENT_TXS_DEPTH,
            );
        }
        if self.config.check_recent_txs_depth > 0 && !self.config.wait_node_sync {
            eyre::bail!("check_recent_txs_depth requires wait_node_sync");
        }
        if self.config.shadow.is_some() && self.config.throttle.is_some() {
            eyre::bail!(
                "shadow mode requires the DA throttle to be disabled: the batcher would push its \
                 DA limits to the sequencer it reads blocks from"
            );
        }

        let signer_config = self
            .config
            .signer
            .clone()
            .ok_or_else(|| eyre::eyre!("signer must be set before starting"))?;
        let signer_address = signer_config.address();

        info!(
            l1_ws = self.config.l1_ws_url.as_ref().map(|u| u.as_str()),
            "starting batcher service"
        );

        let retry = RetryConfig::unbounded(self.config.poll_interval, DEFAULT_UNBOUNDED_MAX_DELAY);
        let rpc_timeout = self.config.wait_node_sync_timeout;

        let l2_provider: Arc<dyn Provider<Base> + Send + Sync> = Arc::new(
            Self::rpc_retry("l2-rpc", retry, rpc_timeout, || {
                ProviderBuilder::new()
                    .disable_recommended_fillers()
                    .network::<Base>()
                    .connect(self.config.l2_rpc_url.as_str())
            })
            .await?,
        );

        // A typed jsonrpsee client calls `optimism_rollupConfig` and `optimism_syncStatus`
        // through the generated `RollupNodeApiClient` trait. Building it only parses the URL,
        // so the rollup config read is the first call that reaches the node.
        let rollup_client = HttpClientBuilder::default()
            .build(self.config.rollup_rpc_url.as_str())
            .map_err(|e| eyre::eyre!("failed to build rollup RPC client: {e}"))?;
        let rollup_config = Arc::new(
            Self::rpc_retry("optimism_rollupConfig", retry, rpc_timeout, || {
                rollup_client.rollup_config()
            })
            .await?,
        );

        // Post to the batch inbox of the rollup node's config, which must be the shadow inbox in
        // shadow mode.
        let batch_inbox = rollup_config.batch_inbox_address;
        if let Some(shadow) = &self.config.shadow {
            shadow.validate_batch_inbox(batch_inbox)?;
            warn!(inbox = %batch_inbox, "shadow mode, posting to the shadow batch inbox");
        } else {
            info!(inbox = %batch_inbox, "rollup config loaded");
        }

        let validator_provider = if let Some(shadow) = &self.config.shadow {
            let provider = Self::rpc_retry("shadow.validator-l2-rpc", retry, rpc_timeout, || {
                ProviderBuilder::new()
                    .disable_recommended_fillers()
                    .network::<Base>()
                    .connect(shadow.validator_l2_rpc.as_str())
            })
            .await?;
            let provider: Arc<dyn Provider<Base> + Send + Sync> = Arc::new(provider);
            Some(RpcL2BlockProvider::new(provider))
        } else {
            None
        };

        // Connect to L1 before the optional node-sync gate.
        let l1_provider: RootProvider = Self::rpc_retry("l1-rpc", retry, rpc_timeout, || {
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect(self.config.l1_rpc_url.as_str())
        })
        .await?;

        // Derivation ignores batches from any other sender, so a wrong signer would only burn L1
        // fees. The shadow batcher posts with its own key on purpose.
        if self.config.shadow.is_none() {
            let system_config = rollup_config.l1_system_config_address;
            let authorized = Self::rpc_retry("system-config-batcher", retry, rpc_timeout, || {
                SystemConfigBatcher::fetch(&l1_provider, system_config)
            })
            .await?;
            if authorized != signer_address {
                eyre::bail!(
                    "signer {signer_address} is not the batcher {authorized} authorized by the L1 \
                     SystemConfig at {system_config}"
                );
            }
        }

        // Recent transactions only select an L1 synchronization target.
        // They never advance the L2 backfill cursor.
        if self.config.wait_node_sync {
            let target_l1 = if self.config.check_recent_txs_depth > 0 {
                Self::rpc_retry("recent-tx-sync-target", retry, rpc_timeout, || {
                    RecentTxSyncTarget::find(
                        &l1_provider,
                        signer_address,
                        self.config.check_recent_txs_depth,
                    )
                })
                .await?
            } else {
                Self::rpc_retry("l1-sync-target", retry, rpc_timeout, || {
                    l1_provider.get_block_number()
                })
                .await?
            };
            Self::wait_for_node_sync(
                &rollup_client,
                target_l1,
                self.config.poll_interval,
                self.config.wait_node_sync_timeout,
            )
            .await?;
        }

        // Channel duration is measured from this tip, not from L1 block 0.
        let initial_l1_head =
            Self::rpc_retry("l1-head", retry, rpc_timeout, || l1_provider.get_block_number())
                .await?;

        let initial_derivation_status =
            Self::rpc_retry("optimism_syncStatus", retry, rpc_timeout, || {
                rollup_client.derivation_status()
            })
            .await?;
        let safe_l2 = initial_derivation_status.safe_l2;
        if safe_l2 == BlockInfo::default() {
            eyre::bail!("safe L2 head is empty");
        }
        let next_l2_timestamp = safe_l2.timestamp.saturating_add(rollup_config.block_time);
        self.config.encoder_config.validate_for_rollup_config(&rollup_config, next_l2_timestamp)?;
        info!(safe_l2 = %safe_l2.number, "fetched safe L2 head");

        if self.config.metrics_enabled {
            let (layer, mut balance_rx) = BalanceMonitorLayer::new(
                signer_address,
                runtime.token().clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            // `layer()` spawns the polling task and moves cloned state into it.
            let _ = layer.layer(l1_provider.clone());
            let balance_cancellation = runtime.token().clone();
            let balance_handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = balance_cancellation.cancelled() => break,
                        changed = balance_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            // Prometheus gauges are f64, so large U256 wei balances lose integer
                            // precision during conversion. This is acceptable for an ether gauge.
                            let balance_ether =
                                f64::from(*balance_rx.borrow_and_update()) / WEI_PER_ETHER;
                            BatcherMetrics::balance().set(balance_ether);
                        }
                    }
                }
            });
            background_tasks.push(("balance monitor relay", balance_handle.err_into().boxed()));
            info!(
                address = %signer_address,
                "batcher balance monitor started"
            );
        }

        if let Some(validator_provider) = validator_provider {
            let handle = L2BlockParityMonitor::new(
                RpcL2BlockProvider::new(Arc::clone(&l2_provider)),
                validator_provider,
                L2BlockParityMonitorConfig::new(
                    safe_l2.number.saturating_add(1),
                    self.config.poll_interval,
                ),
            )
            .spawn(cancellation.clone());
            background_tasks.push(("derived L2 block parity monitor", handle.err_into().boxed()));
        }

        let poller = RpcPollingSource::new(Arc::clone(&l2_provider));
        let source = PollingBlockSource::new(
            TokioRuntime::new(),
            poller,
            safe_l2,
            self.config.poll_interval,
        );
        let encoder =
            BatchEncoder::new(Arc::clone(&rollup_config), self.config.encoder_config.clone())?;

        let throttle = DaThrottle::new(
            self.config.throttle.clone().map_or_else(ThrottleController::disabled, |cfg| {
                ThrottleController::new(cfg, ThrottleStrategy::Linear)
            }),
        );

        // Build the L1 head source: a hybrid of optional WS subscription + polling.
        let l1_head_stream = Self::build_l1_head_stream(self.config.l1_ws_url.as_ref()).await;
        let l1_head_poller = RpcL1HeadPollingSource::new(Arc::new(l1_provider.clone()));
        let l1_head_source = HybridL1HeadSource::new(
            TokioRuntime::new(),
            l1_head_stream,
            l1_head_poller,
            self.config.poll_interval,
        );

        // Fetch L1 chain ID and construct the tx manager.
        let l1_chain_id =
            Self::rpc_retry("l1-chain-id", retry, rpc_timeout, || l1_provider.get_chain_id())
                .await?;
        let drain_timeout = self.config.tx_manager.resubmission_timeout * 2;
        let tx_manager = SimpleTxManager::new(
            l1_provider,
            signer_config,
            self.config.tx_manager,
            l1_chain_id,
            Arc::new(BaseTxMetrics::new("batcher")),
        )
        .await
        .map_err(|e| eyre::eyre!("failed to create tx manager: {e}"))?;

        // Push the DA limits to the L2 endpoint, whose block builder applies them. A disabled
        // throttle has nothing to push.
        if self.config.throttle.is_some() {
            let pusher = ThrottlePusher::new(&self.config.l2_rpc_url, throttle.subscribe())?;
            let handle = tokio::spawn(pusher.run(runtime.clone()));
            background_tasks
                .push(("throttle pusher", handle.err_into().map(Result::flatten).boxed()));
        }

        let (derivation_status_tx, derivation_status_rx) = mpsc::channel(1);

        let derivation_status_handle = tokio::spawn(
            DerivationStatusPoller::new(
                rollup_client,
                self.config.poll_interval,
                initial_derivation_status,
                derivation_status_tx,
            )
            .run(runtime.clone()),
        );
        background_tasks
            .push(("derivation status poller", derivation_status_handle.err_into().boxed()));

        // Build the driver.
        let (admin_handle, admin_rx) = AdminHandle::channel();
        let driver = BatchDriver::new(
            runtime,
            encoder,
            tx_manager,
            base_batcher_core::BatchDriverConfig {
                inbox: batch_inbox,
                max_pending_transactions: self.config.max_pending_transactions,
                drain_timeout,
                force_blobs_when_throttling: self.config.force_blobs_when_throttling,
                stopped: self.config.stopped,
            },
            throttle,
            BatchDriverInputs {
                source,
                l1_head_source,
                initial_l1_head,
                initial_safe_head: safe_l2,
                derivation_status_rx,
                admin_rx,
            },
        );

        // Without an admin server, drop the handle: the driver's admin arm then stays quiet.
        let admin_server = match self.config.admin_addr {
            Some(addr) => Some(AdminServer::spawn(addr, admin_handle).await?),
            None => {
                drop(admin_handle);
                None
            }
        };

        info!("batcher service components initialized");
        Ok(ReadyBatcher { driver, admin_server, background_tasks, cancellation })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, Ordering};

    use alloy_node_bindings::Anvil;
    use alloy_primitives::Address;
    use base_batcher_core::ThrottleConfig;
    use base_common_genesis::RollupConfig;
    use base_protocol::SyncStatus;
    use base_tx_manager::SignerConfig;
    use httpmock::{Mock, prelude::*};
    use rstest::rstest;

    use super::*;
    use crate::ShadowConfig;

    /// The `SystemConfig` address of the mocked rollup config.
    const SYSTEM_CONFIG: Address = Address::repeat_byte(0x5c);

    /// The batch inbox of the mocked rollup config, which a shadow batcher following that rollup
    /// node declares as its shadow inbox.
    const BATCH_INBOX: Address = Address::repeat_byte(0x1b);

    fn test_retry() -> RetryConfig {
        RetryConfig::unbounded(Duration::from_millis(1), Duration::from_millis(1))
    }

    /// Answers every JSON-RPC request whose body includes `request` with `result`, under id 0,
    /// the id of each client's first request.
    async fn mock_rpc<'a>(server: &'a MockServer, request: &str, result: String) -> Mock<'a> {
        let response = format!(r#"{{"jsonrpc":"2.0","id":0,"result":{result}}}"#);
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").json_body_includes(request);
                then.status(200).header("content-type", "application/json").body(response);
            })
            .await
    }

    /// A batcher whose L1, L2 and rollup endpoints are all `server`, signing as `signer`.
    fn mocked_config(server: &MockServer, signer: Address) -> BatcherConfig {
        let url: Url = server.url("/").parse().unwrap();
        BatcherConfig {
            l1_rpc_url: url.clone(),
            l2_rpc_url: url.clone(),
            rollup_rpc_url: url.clone(),
            signer: Some(SignerConfig::Remote { endpoint: url, address: signer }),
            poll_interval: Duration::from_millis(10),
            wait_node_sync_timeout: Duration::from_millis(200),
            ..BatcherConfig::default()
        }
    }

    /// Serves a rollup config whose `SystemConfig` authorizes `authorized`, and returns the
    /// mock of the `batcherHash()` call.
    async fn mock_system_config(server: &MockServer, authorized: Address) -> Mock<'_> {
        let rollup_config = RollupConfig {
            batch_inbox_address: BATCH_INBOX,
            l1_system_config_address: SYSTEM_CONFIG,
            ..RollupConfig::default()
        };
        mock_rpc(
            server,
            r#"{"method":"optimism_rollupConfig"}"#,
            serde_json::to_string(&rollup_config).unwrap(),
        )
        .await;
        mock_rpc(server, r#"{"method":"eth_call"}"#, format!(r#""{}""#, authorized.into_word()))
            .await
    }

    /// A startup RPC read is retried until it succeeds.
    #[tokio::test]
    async fn rpc_retry_succeeds_after_transient_failure() {
        let attempts = AtomicU8::new(0);
        let value = BatcherService::rpc_retry("test", test_retry(), Duration::from_secs(1), || {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move { if n < 2 { Err("transient") } else { Ok(7u64) } }
        })
        .await
        .expect("retry should succeed after transient failures");
        assert_eq!(value, 7);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    /// A startup RPC read that keeps failing gives up at its timeout, naming the operation.
    #[tokio::test]
    async fn rpc_retry_times_out_while_failing() {
        let error = BatcherService::rpc_retry(
            "test-op",
            test_retry(),
            Duration::from_millis(20),
            || async { Err::<(), _>("always") },
        )
        .await
        .expect_err("retry should time out while the RPC keeps failing");
        assert_eq!(error.to_string(), "test-op timed out after 20ms");
    }

    /// Setup refuses a config the batcher cannot run before connecting to anything.
    #[rstest]
    #[case::zero_poll_interval(
        BatcherConfig { poll_interval: Duration::ZERO, ..BatcherConfig::default() },
        "poll_interval must be greater than zero"
    )]
    #[case::zero_max_pending_transactions(
        BatcherConfig { max_pending_transactions: 0, ..BatcherConfig::default() },
        "max_pending_transactions must be greater than zero: the batcher would never submit a \
         transaction"
    )]
    #[case::stopped_without_admin_server(
        BatcherConfig { stopped: true, ..BatcherConfig::default() },
        "--stopped requires --admin-port: the batcher would start stopped with no way to start \
         it because the admin JSON-RPC server is not enabled"
    )]
    #[case::invalid_throttle(
        BatcherConfig {
            throttle: Some(ThrottleConfig { block_size_lower_limit: 0, ..ThrottleConfig::default() }),
            ..BatcherConfig::default()
        },
        "block_size_lower_limit must be greater than zero"
    )]
    #[case::recent_txs_depth_above_the_cap(
        BatcherConfig { check_recent_txs_depth: 129, wait_node_sync: true, ..BatcherConfig::default() },
        "check_recent_txs_depth 129 exceeds maximum of 128"
    )]
    #[case::no_signer(BatcherConfig::default(), "signer must be set before starting")]
    #[case::recent_txs_without_node_sync(
        BatcherConfig { check_recent_txs_depth: 1, ..BatcherConfig::default() },
        "check_recent_txs_depth requires wait_node_sync"
    )]
    #[case::shadow_with_throttle(
        BatcherConfig {
            shadow: Some(ShadowConfig {
                inbox: Address::ZERO,
                validator_l2_rpc: "http://127.0.0.1:1".parse().unwrap(),
            }),
            throttle: Some(ThrottleConfig::default()),
            ..BatcherConfig::default()
        },
        "shadow mode requires the DA throttle to be disabled: the batcher would push its DA \
         limits to the sequencer it reads blocks from"
    )]
    #[tokio::test]
    async fn setup_refuses_a_config_it_cannot_run(
        #[case] config: BatcherConfig,
        #[case] expected: &str,
    ) {
        let error = BatcherService::new(config).setup(TokioRuntime::new()).await.unwrap_err();

        assert_eq!(error.to_string(), expected);
    }

    /// Startup goes on once the rollup node has processed the L1 target, and gives up at the
    /// timeout while the node is behind it.
    #[rstest]
    #[case::reached(5, Ok(()))]
    #[case::behind(6, Err("wait_for_node_sync timed out"))]
    #[tokio::test]
    async fn wait_for_node_sync_waits_for_the_l1_target(
        #[case] target_l1: u64,
        #[case] expected: Result<(), &str>,
    ) {
        let server = MockServer::start_async().await;
        let status = SyncStatus {
            current_l1: BlockInfo { number: 5, ..Default::default() },
            ..Default::default()
        };
        mock_rpc(
            &server,
            r#"{"method":"optimism_syncStatus"}"#,
            serde_json::to_string(&status).unwrap(),
        )
        .await;
        let client = HttpClientBuilder::default().build(server.url("/")).unwrap();

        let result = BatcherService::wait_for_node_sync(
            &client,
            target_l1,
            Duration::from_millis(10),
            Duration::from_millis(100),
        )
        .await;

        assert_eq!(result.map_err(|error| error.to_string()), expected.map_err(String::from));
    }

    /// Setup refuses a signer the L1 `SystemConfig` does not authorize, since derivation would
    /// ignore its batches.
    #[tokio::test]
    async fn setup_rejects_a_signer_the_system_config_does_not_authorize() {
        let server = MockServer::start_async().await;
        let (signer, authorized) = (Address::repeat_byte(0x51), Address::repeat_byte(0xba));
        mock_system_config(&server, authorized).await;

        let error = BatcherService::new(mocked_config(&server, signer))
            .setup(TokioRuntime::new())
            .await
            .expect_err("a batcher whose batches derivation ignores must not start");

        assert_eq!(
            error.to_string(),
            format!(
                "signer {signer} is not the batcher {authorized} authorized by the L1 \
                 SystemConfig at {SYSTEM_CONFIG}"
            )
        );
    }

    /// Setup goes past the batcher check when the signer is the one the `SystemConfig` authorizes.
    #[tokio::test]
    async fn setup_accepts_the_signer_the_system_config_authorizes() {
        let server = MockServer::start_async().await;
        let signer = Address::repeat_byte(0x51);
        mock_system_config(&server, signer).await;
        // Setup reads the L1 head right after the check, so a served read means the check passed.
        let l1_head = mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#, r#""0x1""#.into()).await;

        // Setup fails later, on the reads this test does not mock.
        let _ =
            BatcherService::new(mocked_config(&server, signer)).setup(TokioRuntime::new()).await;

        assert!(l1_head.calls_async().await > 0, "setup must go past the check");
    }

    /// A shadow batcher posts to its own inbox, so setup does not check its signer against the
    /// `SystemConfig`.
    #[tokio::test]
    async fn setup_skips_the_batcher_check_in_shadow_mode() {
        let server = MockServer::start_async().await;
        let batcher_hash = mock_system_config(&server, Address::repeat_byte(0xba)).await;
        // Setup reads the L1 head right after the check, so a served read means setup got that far.
        let l1_head = mock_rpc(&server, r#"{"method":"eth_blockNumber"}"#, r#""0x1""#.into()).await;
        let config = BatcherConfig {
            shadow: Some(ShadowConfig {
                inbox: BATCH_INBOX,
                validator_l2_rpc: server.url("/").parse().unwrap(),
            }),
            throttle: None,
            ..mocked_config(&server, Address::repeat_byte(0x51))
        };

        // Setup fails later, on the reads this test does not mock.
        let _ = BatcherService::new(config).setup(TokioRuntime::new()).await;

        assert!(l1_head.calls_async().await > 0, "setup must reach the L1 head read");
        batcher_hash.assert_calls_async(0).await;
    }

    /// The L1 head stream keeps its WebSocket provider alive after the builder returns, so new L1
    /// heads keep arriving.
    #[tokio::test]
    async fn l1_head_stream_outlives_its_builder() {
        let anvil = Anvil::new().spawn();
        let mut heads = BatcherService::build_l1_head_stream(Some(&anvil.ws_endpoint_url())).await;

        // The builder has returned, so the stream alone must keep the WS provider alive.
        let miner = RootProvider::<Base>::new_http(anvil.endpoint_url());
        for expected in 1..=2 {
            miner.raw_request::<(), String>("evm_mine".into(), ()).await.unwrap();
            let head = tokio::time::timeout(Duration::from_secs(5), heads.next()).await;
            assert_eq!(head.unwrap(), Some(expected));
        }
    }
}
