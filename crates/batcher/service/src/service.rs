//! Batcher service startup and wiring.

use std::{
    future::{Future, pending},
    sync::Arc,
    time::Duration,
};

use alloy_provider::{Provider, ProviderBuilder, ProviderLayer, RootProvider};
use backon::Retryable;
use base_balance_monitor::BalanceMonitorLayer;
use base_batcher_admin::AdminServer;
use base_batcher_core::{
    AdminHandle, BatchDriver, BatchDriverHeads, DaThrottle, NoopThrottleClient, ThrottleClient,
    ThrottleConfig, ThrottleController, ThrottleStrategy,
};
use base_batcher_encoder::{BatchEncoder, BatcherMetrics};
use base_batcher_source::{HybridL1HeadSource, PollingBlockSource, SourceError};
use base_common_network::Base;
use base_consensus_rpc::RollupNodeApiClient;
use base_protocol::BlockInfo;
use base_retry::{DEFAULT_UNBOUNDED_MAX_DELAY, RetryConfig};
use base_runtime::TokioRuntime;
use base_tx_manager::{BaseTxMetrics, SimpleTxManager};
use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{BoxStream, FuturesUnordered},
};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    BatcherConfig, DerivationStatusPoller, DerivationStatusProvider, L2BlockParityMonitor,
    L2BlockParityMonitorConfig, MAX_CHECK_RECENT_TXS_DEPTH, NullL1HeadSubscription,
    RecentTxSyncTarget, RpcL1HeadPollingSource, RpcL2BlockProvider, RpcPollingSource,
    RpcThrottleClient, WsL1HeadSubscription,
};

const WEI_PER_ETHER: f64 = 1_000_000_000_000_000_000.0;

/// Service-internal throttle client variant: either a no-op or an RPC client.
///
/// Using a concrete enum avoids heap allocation while still allowing
/// `start` to return either branch based on config.
enum ServiceThrottle {
    Noop(NoopThrottleClient),
    Rpc(RpcThrottleClient),
}

impl ThrottleClient for ServiceThrottle {
    fn set_max_da_size(
        &self,
        max_tx_size: u64,
        max_block_size: u64,
    ) -> BoxFuture<'_, Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        match self {
            Self::Noop(n) => n.set_max_da_size(max_tx_size, max_block_size),
            Self::Rpc(r) => r.set_max_da_size(max_tx_size, max_block_size),
        }
    }
}

/// Batcher-internal L1 subscription variant: either a live WS subscription or a no-op.
enum L1Subscription {
    Ws(WsL1HeadSubscription),
    Null(NullL1HeadSubscription),
}

impl base_batcher_source::L1HeadSubscription for L1Subscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<u64, SourceError>> {
        match self {
            Self::Ws(ws) => ws.take_stream(),
            Self::Null(null) => null.take_stream(),
        }
    }
}

/// Concrete driver type produced by [`BatcherService::setup`].
///
/// Private — callers interact only through [`ReadyBatcher`].
type ServiceDriver = BatchDriver<
    TokioRuntime,
    BatchEncoder,
    PollingBlockSource<RpcPollingSource, TokioRuntime>,
    SimpleTxManager<RootProvider>,
    ServiceThrottle,
    HybridL1HeadSource<L1Subscription, RpcL1HeadPollingSource, TokioRuntime>,
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
    background_tasks: Vec<(&'static str, JoinHandle<()>)>,
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
                .map(|(task_name, handle)| async move { (task_name, handle.await) })
                .collect::<FuturesUnordered<_>>();
            tokio::select! {
                biased;
                () = background_cancellation.cancelled() => {}
                Some((task_name, result)) = background_tasks.next(), if !background_tasks.is_empty() => {
                    match result {
                        Ok(()) => {
                            eyre::bail!("{task_name} exited unexpectedly")
                        }
                        Err(error) => {
                            eyre::bail!("{task_name} task failed: {error}")
                        }
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

    /// Build an L1 head subscription for the given optional L1 WebSocket URL.
    ///
    /// When `url` is `Some`, connects a dedicated WS provider, subscribes to
    /// new L1 block headers, and streams their block numbers. The provider is
    /// wrapped in a [`WsL1HeadSubscription`] to keep the connection alive.
    ///
    /// When `url` is `None`, or if the WS connection fails, returns a
    /// [`NullL1HeadSubscription`] so that [`HybridL1HeadSource`] falls back
    /// entirely to polling.
    ///
    /// [`HybridL1HeadSource`]: base_batcher_source::HybridL1HeadSource
    async fn build_l1_subscription(url: Option<&Url>) -> L1Subscription {
        let Some(url) = url else {
            return L1Subscription::Null(NullL1HeadSubscription::new());
        };

        let ws_provider = match ProviderBuilder::new().connect(url.as_str()).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn!(error = %e, l1_ws = %url, "failed to connect L1 WS provider; falling back to polling");
                return L1Subscription::Null(NullL1HeadSubscription::new());
            }
        };

        let sub = match ws_provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to subscribe to new L1 blocks; falling back to polling");
                return L1Subscription::Null(NullL1HeadSubscription::new());
            }
        };

        let stream = sub.into_stream().map(|header| Ok(header.number)).boxed();
        L1Subscription::Ws(WsL1HeadSubscription::new(ws_provider, stream))
    }

    /// Try each URL in order, returning the first that connects.
    ///
    /// Logs each failed attempt with the endpoint that produced it so operators
    /// can tell whether failover occurred. Returns an error containing the last
    /// failure if every endpoint fails. The list must be non-empty.
    async fn connect_first<T, F, Fut, E>(
        urls: &[Url],
        label: &'static str,
        mut build: F,
    ) -> eyre::Result<T>
    where
        F: FnMut(&Url) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut last_err: Option<String> = None;
        for url in urls {
            match build(url).await {
                Ok(t) => {
                    info!(endpoint = %label, url = %url, "connected to endpoint");
                    return Ok(t);
                }
                Err(e) => {
                    warn!(endpoint = %label, url = %url, error = %e, "endpoint connection failed, trying next");
                    last_err = Some(e.to_string());
                }
            }
        }
        Err(eyre::eyre!(
            "failed to connect to any {label} endpoint ({} candidate(s)): {}",
            urls.len(),
            last_err.unwrap_or_else(|| "no candidates".to_string()),
        ))
    }

    /// Retry a one-shot startup RPC until it succeeds or `timeout` elapses.
    ///
    /// Uses [`RetryConfig`] for exponential backoff with jitter. URL failover
    /// stays in [`connect_first`]: this retries the whole attempt, including
    /// walking the endpoint list again.
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
    /// Connects to the L2 and L1 RPC endpoints, fetches the rollup config,
    /// validates the private key, and constructs the driver. One-shot startup
    /// RPCs retry with exponential backoff until
    /// [`BatcherConfig::wait_node_sync_timeout`]. Returns an error if any of
    /// those steps fail — the caller sees the failure immediately, before any
    /// background work is spawned.
    ///
    /// The runtime's cancellation token is forwarded to the derivation-status poller
    /// spawned here so it stops cleanly when the batcher shuts down.
    pub async fn setup(self, runtime: TokioRuntime) -> eyre::Result<ReadyBatcher> {
        let cancellation = runtime.token().clone();
        let mut background_tasks = Vec::new();
        self.config.encoder_config.validate()?;

        if self.config.poll_interval.is_zero() {
            eyre::bail!("poll_interval must be greater than zero");
        }
        if self.config.stopped && self.config.admin_addr.is_none() {
            eyre::bail!(
                "--stopped requires --admin-port: the batcher would start stopped with no way to \
                 resume because the admin JSON-RPC server is not enabled"
            );
        }
        if self.config.l1_rpc_url.is_empty() {
            eyre::bail!("at least one L1 RPC endpoint is required");
        }
        if self.config.l2_rpc_url.is_empty() {
            eyre::bail!("at least one L2 RPC endpoint is required");
        }
        if self.config.rollup_rpc_url.is_empty() {
            eyre::bail!("at least one rollup RPC endpoint is required");
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
        match (self.config.batch_inbox_override, self.config.parity_validator_l2_rpc_url.as_ref()) {
            (None, Some(_)) => {
                eyre::bail!("parity validator L2 RPC URL requires shadow mode")
            }
            (Some(_), None) => {
                eyre::bail!(
                    "shadow mode requires a parity validator L2 RPC URL for its safe L2 head"
                )
            }
            _ => {}
        }

        let signer_config = self
            .config
            .signer
            .clone()
            .ok_or_else(|| eyre::eyre!("signer must be set before starting"))?;
        let signer_address = signer_config.address();

        info!(
            l1_rpc_count = self.config.l1_rpc_url.len(),
            l2_rpc_count = self.config.l2_rpc_url.len(),
            rollup_rpc_count = self.config.rollup_rpc_url.len(),
            l1_ws = self.config.l1_ws_url.as_ref().map(|u| u.as_str()),
            "starting batcher service"
        );

        let retry = RetryConfig::unbounded(self.config.poll_interval, DEFAULT_UNBOUNDED_MAX_DELAY);
        let rpc_timeout = self.config.wait_node_sync_timeout;

        // Connect to the L2 RPC endpoint, with connection-time failover across
        // the configured endpoint list.
        let l2_provider: Arc<dyn Provider<Base> + Send + Sync> = Arc::new(
            Self::rpc_retry("l2-rpc", retry, rpc_timeout, || {
                Self::connect_first(&self.config.l2_rpc_url, "l2-rpc", |url| {
                    let url = url.clone();
                    async move {
                        ProviderBuilder::new()
                            .disable_recommended_fillers()
                            .network::<Base>()
                            .connect(url.as_str())
                            .await
                    }
                })
            })
            .await?,
        );

        // Connect to the rollup node using a typed jsonrpsee HTTP client so that
        // `optimism_rollupConfig` and `optimism_syncStatus` are called through the
        // generated `RollupNodeApiClient` trait rather than raw JSON requests.
        // `HttpClientBuilder::build` is sync but only validates the URL; the first
        // real RPC (`rollup_config`) is what actually exercises the endpoint, so
        // that call both drives failover and supplies the config used below.
        let (rollup_client, rollup_config) =
            Self::rpc_retry("rollup-rpc", retry, rpc_timeout, || {
                Self::connect_first(&self.config.rollup_rpc_url, "rollup-rpc", |url| {
                    let url = url.clone();
                    async move {
                        let client = HttpClientBuilder::default()
                            .build(url.as_str())
                            .map_err(|e| eyre::eyre!("failed to build rollup RPC client: {e}"))?;
                        let config = client
                            .rollup_config()
                            .await
                            .map_err(|e| eyre::eyre!("optimism_rollupConfig RPC failed: {e}"))?;
                        eyre::Ok((client, config))
                    }
                })
            })
            .await?;
        let rollup_config = Arc::new(rollup_config);
        let effective_batch_inbox =
            self.config.batch_inbox_override.unwrap_or(rollup_config.batch_inbox_address);
        if self.config.batch_inbox_override.is_some() {
            warn!(
                configured_inbox = %effective_batch_inbox,
                rollup_config_inbox = %rollup_config.batch_inbox_address,
                "using dangerous shadow batch inbox override"
            );
        } else {
            info!(
                inbox = %effective_batch_inbox,
                "rollup config loaded"
            );
        }

        let validator_provider = if let Some(url) = &self.config.parity_validator_l2_rpc_url {
            let url = url.clone();
            let provider = Self::rpc_retry("parity-validator-l2-rpc", retry, rpc_timeout, || {
                let url = url.clone();
                async move {
                    ProviderBuilder::new()
                        .disable_recommended_fillers()
                        .network::<Base>()
                        .connect(url.as_str())
                        .await
                }
            })
            .await?;
            let provider: Arc<dyn Provider<Base> + Send + Sync> = Arc::new(provider);
            Some(RpcL2BlockProvider::new(provider))
        } else {
            None
        };

        // Connect to L1 before the optional node-sync gate.
        let l1_provider: RootProvider = Self::rpc_retry("l1-rpc", retry, rpc_timeout, || {
            Self::connect_first(&self.config.l1_rpc_url, "l1-rpc", |url| {
                let url = url.clone();
                async move {
                    ProviderBuilder::new().disable_recommended_fillers().connect(url.as_str()).await
                }
            })
        })
        .await?;

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

        let initial_derivation_status = if let Some(provider) = validator_provider.as_ref() {
            Self::rpc_retry("parity-validator-safe-l2", retry, rpc_timeout, || {
                provider.derivation_status()
            })
            .await?
        } else {
            Self::rpc_retry("optimism_syncStatus", retry, rpc_timeout, || {
                rollup_client.derivation_status()
            })
            .await?
        };
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
            background_tasks.push(("balance monitor relay", balance_handle));
            info!(
                address = %signer_address,
                "batcher balance monitor started"
            );
        }

        if let Some(validator_provider) = validator_provider.as_ref() {
            let handle = L2BlockParityMonitor::new(
                RpcL2BlockProvider::new(Arc::clone(&l2_provider)),
                validator_provider.clone(),
                L2BlockParityMonitorConfig::new(
                    safe_l2.number.saturating_add(1),
                    self.config.poll_interval,
                ),
            )
            .spawn(cancellation.clone());
            background_tasks.push(("derived L2 block parity monitor", handle));
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

        // Build the throttle controller and the appropriate client. The throttle
        // RPC uses the L2 endpoint(s); `RpcThrottleClient` rotates per-call
        // across the full L2 endpoint list so a single dead L2 RPC does not
        // silently disable throttle delivery to the sequencer.
        let throttle_client = match &self.config.throttle {
            None => ServiceThrottle::Noop(NoopThrottleClient),
            Some(_) => {
                let urls: Vec<&str> = self.config.l2_rpc_url.iter().map(Url::as_str).collect();
                ServiceThrottle::Rpc(RpcThrottleClient::new(&urls)?)
            }
        };
        let (throttle_config, throttle_strategy) = self.config.throttle.clone().map_or_else(
            || (ThrottleConfig::default(), ThrottleStrategy::Off),
            |cfg| (cfg, ThrottleStrategy::Linear),
        );
        let throttle = ThrottleController::new(throttle_config, throttle_strategy);

        // Build the L1 head source: a hybrid of optional WS subscription + polling.
        let l1_head_subscription =
            Self::build_l1_subscription(self.config.l1_ws_url.as_ref()).await;
        let l1_head_poller = RpcL1HeadPollingSource::new(Arc::new(
            Self::rpc_retry("l1-rpc-poller", retry, rpc_timeout, || {
                Self::connect_first(&self.config.l1_rpc_url, "l1-rpc-poller", |url| {
                    let url = url.clone();
                    async move {
                        ProviderBuilder::new()
                            .disable_recommended_fillers()
                            .connect(url.as_str())
                            .await
                    }
                })
            })
            .await?,
        ));
        let l1_head_source = HybridL1HeadSource::new(
            TokioRuntime::new(),
            l1_head_subscription,
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

        let (derivation_status_tx, derivation_status_rx) = mpsc::channel(1);

        // Canonical mode follows the rollup node's LocalSafeL2. Shadow mode
        // follows the parity validator's safe label so canonical DA progress
        // cannot cause shadow-only gaps to be skipped.
        let derivation_status_handle = if let Some(provider) = validator_provider {
            tokio::spawn(
                DerivationStatusPoller::new(
                    provider,
                    self.config.poll_interval,
                    initial_derivation_status,
                    derivation_status_tx,
                )
                .run(runtime.clone()),
            )
        } else {
            tokio::spawn(
                DerivationStatusPoller::new(
                    rollup_client,
                    self.config.poll_interval,
                    initial_derivation_status,
                    derivation_status_tx,
                )
                .run(runtime.clone()),
            )
        };
        background_tasks.push(("derivation status poller", derivation_status_handle));

        // Build the driver — all fallible setup is complete at this point.
        let mut driver = BatchDriver::new(
            runtime,
            encoder,
            source,
            tx_manager,
            base_batcher_core::BatchDriverConfig {
                inbox: effective_batch_inbox,
                max_pending_transactions: self.config.max_pending_transactions,
                drain_timeout,
                force_blobs_when_throttling: self.config.force_blobs_when_throttling,
            },
            DaThrottle::new(throttle, throttle_client),
            BatchDriverHeads::new(
                l1_head_source,
                initial_l1_head,
                initial_derivation_status,
                derivation_status_rx,
            ),
        )
        .with_stopped(self.config.stopped);

        let admin_server = match self.config.admin_addr {
            Some(addr) => {
                let (admin_handle, admin_rx) = AdminHandle::channel();
                driver = driver.with_admin_rx(admin_rx);
                Some(AdminServer::spawn(addr, admin_handle).await?)
            }
            None => None,
        };

        info!("batcher service components initialized");
        Ok(ReadyBatcher { driver, admin_server, background_tasks, cancellation })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::*;

    fn test_retry() -> RetryConfig {
        RetryConfig::unbounded(Duration::from_millis(1), Duration::from_millis(1))
    }

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
        assert!(
            error.to_string().contains("test-op"),
            "timeout error should name the operation, got {error}"
        );
    }
}
