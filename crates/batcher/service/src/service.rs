//! Batcher service startup and wiring.

use std::{future::pending, sync::Arc};

use alloy_provider::{Provider, ProviderBuilder, ProviderLayer, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_balance_monitor::BalanceMonitorLayer;
use base_batcher_admin::AdminServer;
use base_batcher_core::{
    AdminHandle, BatchDriver, DaThrottle, NoopThrottleClient, ThrottleClient, ThrottleConfig,
    ThrottleController, ThrottleStrategy,
};
use base_batcher_encoder::{BatchEncoder, BatcherMetrics};
use base_batcher_source::{BlockSubscription, HybridBlockSource, HybridL1HeadSource, SourceError};
use base_common_consensus::BaseBlock;
use base_common_network::Base;
use base_consensus_rpc::RollupNodeApiClient;
use base_runtime::TokioRuntime;
use base_tx_manager::{BaseTxMetrics, SimpleTxManager, TxManagerConfig};
use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{BoxStream, FuturesUnordered},
};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    BatcherConfig, L2BlockParityMonitor, L2BlockParityMonitorConfig, MAX_CHECK_RECENT_TXS_DEPTH,
    NullL1HeadSubscription, NullSubscription, RecentTxScanner, RpcL1HeadPollingSource,
    RpcL2BlockProvider, RpcPollingSource, RpcThrottleClient, SafeHeadPoller, ShadowParityMonitor,
    ShadowParityMonitorConfig, WsBlockSubscription, WsL1HeadSubscription,
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

/// Batcher-internal L2 subscription variant: either a live WS subscription or a no-op.
///
/// Using a concrete enum avoids heap allocation while still allowing
/// `build_subscription` to return either branch to `start`.
enum Subscription {
    Ws(WsBlockSubscription),
    Null(NullSubscription),
}

impl BlockSubscription for Subscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<BaseBlock, SourceError>> {
        match self {
            Self::Ws(ws) => ws.take_stream(),
            Self::Null(null) => null.take_stream(),
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
    HybridBlockSource<Subscription, RpcPollingSource, TokioRuntime>,
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
                        Ok(()) => eyre::bail!("{task_name} exited unexpectedly"),
                        Err(error) => eyre::bail!("{task_name} task failed: {error}"),
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

    /// Build a block subscription for the given optional L2 WebSocket URL.
    ///
    /// When `url` is `Some`, connects a dedicated WS provider, subscribes to
    /// new block headers, and builds a stream that fetches the full block for
    /// each header. The provider is wrapped in a [`WsBlockSubscription`] so its
    /// lifetime is tied to the returned subscription — and therefore to the
    /// [`HybridBlockSource`] that consumes it — rather than to this function's
    /// stack frame.
    ///
    /// When `url` is `None`, or if the WS connection fails, returns a
    /// [`NullSubscription`] so that [`HybridBlockSource`] falls back entirely
    /// to polling.
    ///
    /// [`HybridBlockSource`]: base_batcher_source::HybridBlockSource
    async fn build_l2_subscription(
        url: Option<&Url>,
        fetch_provider: Arc<dyn Provider<Base> + Send + Sync>,
    ) -> Subscription {
        let Some(url) = url else {
            return Subscription::Null(NullSubscription::new());
        };

        let ws_provider = match ProviderBuilder::new().connect(url.as_str()).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn!(error = %e, l2_rpc = %url, "failed to connect L2 WS provider; falling back to polling");
                return Subscription::Null(NullSubscription::new());
            }
        };

        let sub = match ws_provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to subscribe to new L2 blocks; falling back to polling");
                return Subscription::Null(NullSubscription::new());
            }
        };

        let stream = sub
            .into_stream()
            .then(move |header| {
                let provider = Arc::clone(&fetch_provider);
                async move {
                    let rpc_block = provider
                        .get_block_by_number(BlockNumberOrTag::Number(header.number))
                        .full()
                        .await
                        .map_err(|e| SourceError::Provider(e.to_string()))?
                        .ok_or_else(|| {
                            SourceError::Provider(format!("block {} not found", header.number))
                        })?;
                    let block = rpc_block
                        .map_header(|header| header.into_inner())
                        .into_consensus()
                        .map_transactions(|t| t.inner.into_inner());
                    Ok(block)
                }
            })
            .boxed();

        Subscription::Ws(WsBlockSubscription::new(ws_provider, stream))
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

    /// Block until the rollup node reports a non-zero sync status, or until
    /// `timeout` elapses.
    ///
    /// Polls `optimism_syncStatus` on `poll_interval` and returns once both
    /// `current_l1.number` and `unsafe_l2.block_info.number` are non-zero.
    /// RPC errors are logged and retried with exponential backoff (capped at
    /// 30 seconds) so a permanently-broken endpoint is not hammered at the
    /// poll cadence. Returns an error when `timeout` is exceeded so operators
    /// see an explicit failure rather than a silent hang.
    async fn wait_for_node_sync(
        rollup_client: &HttpClient,
        poll_interval: std::time::Duration,
        timeout: std::time::Duration,
    ) -> eyre::Result<()> {
        // Cap RPC-error backoff so a broken endpoint backs off but eventually
        // recovers within a reasonable window.
        const MAX_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

        info!(
            timeout_secs = %timeout.as_secs(),
            "waiting for rollup node to report a non-zero sync status"
        );
        let deadline = std::time::Instant::now() + timeout;
        let mut error_backoff = poll_interval;
        loop {
            match rollup_client.sync_status().await {
                Ok(status)
                    if status.current_l1.number > 0 && status.unsafe_l2.block_info.number > 0 =>
                {
                    info!(
                        current_l1 = %status.current_l1.number,
                        unsafe_l2 = %status.unsafe_l2.block_info.number,
                        safe_l2 = %status.safe_l2.block_info.number,
                        "rollup node reports sync, proceeding with batcher startup"
                    );
                    return Ok(());
                }
                Ok(status) => {
                    // Reset error backoff: the RPC is responsive, the node
                    // just hasn't produced/derived blocks yet.
                    error_backoff = poll_interval;
                    info!(
                        current_l1 = %status.current_l1.number,
                        unsafe_l2 = %status.unsafe_l2.block_info.number,
                        "rollup node not yet synced, waiting"
                    );
                    Self::sleep_or_timeout(poll_interval, deadline).await?;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        backoff_secs = %error_backoff.as_secs(),
                        "optimism_syncStatus RPC failed during wait, backing off"
                    );
                    Self::sleep_or_timeout(error_backoff, deadline).await?;
                    error_backoff = (error_backoff * 2).min(MAX_ERROR_BACKOFF);
                }
            }
        }
    }

    /// Sleep for `dur` or until `deadline`, whichever is sooner.
    ///
    /// Returns `Err` if the deadline is reached before or during the sleep so
    /// callers surface a single timeout error rather than silently looping
    /// past the deadline.
    async fn sleep_or_timeout(
        dur: std::time::Duration,
        deadline: std::time::Instant,
    ) -> eyre::Result<()> {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(eyre::eyre!(
                "wait_for_node_sync timed out before the rollup node reported a non-zero sync status"
            ));
        }
        let remaining = deadline - now;
        tokio::time::sleep(dur.min(remaining)).await;
        if std::time::Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "wait_for_node_sync timed out before the rollup node reported a non-zero sync status"
            ));
        }
        Ok(())
    }

    /// Initialise all batcher components and return a [`ReadyBatcher`].
    ///
    /// Connects to the L2 and L1 RPC endpoints, fetches the rollup config,
    /// validates the private key, and constructs the driver. Returns an error
    /// if any of those steps fail — the caller sees the failure immediately,
    /// before any background work is spawned.
    ///
    /// The runtime's cancellation token is forwarded to the safe-head poller
    /// spawned here so it stops cleanly when the batcher shuts down.
    pub async fn setup(self, runtime: TokioRuntime) -> eyre::Result<ReadyBatcher> {
        let cancellation = runtime.token().clone();
        let mut background_tasks = Vec::new();
        self.config.encoder_config.validate()?;

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
            l2_ws = self.config.l2_ws_url.as_ref().map(|u| u.as_str()),
            l1_ws = self.config.l1_ws_url.as_ref().map(|u| u.as_str()),
            "starting batcher service"
        );

        // Connect to the L2 RPC endpoint, with connection-time failover across
        // the configured endpoint list.
        let l2_provider: Arc<dyn Provider<Base> + Send + Sync> = Arc::new(
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
            .await?,
        );

        // Build the L2 block subscription. When l2_ws_url is configured the
        // subscription owns its provider Arc so the connection stays live for
        // the full driver run.
        let l2_subscription =
            Self::build_l2_subscription(self.config.l2_ws_url.as_ref(), Arc::clone(&l2_provider))
                .await;

        // Connect to the rollup node using a typed jsonrpsee HTTP client so that
        // `optimism_rollupConfig` and `optimism_syncStatus` are called through the
        // generated `RollupNodeApiClient` trait rather than raw JSON requests.
        // `HttpClientBuilder::build` is sync but only validates the URL; the first
        // real RPC call below (`rollup_config`) is what actually exercises the
        // endpoint, so we probe via `rollup_config` to drive failover.
        let rollup_client: HttpClient =
            Self::connect_first(&self.config.rollup_rpc_url, "rollup-rpc", |url| {
                let url = url.clone();
                async move {
                    let client = HttpClientBuilder::default()
                        .build(url.as_str())
                        .map_err(|e| eyre::eyre!("failed to build rollup RPC client: {e}"))?;
                    // Issue a cheap probe call so a non-responsive endpoint
                    // triggers failover instead of falling through to the next
                    // step and erroring with no fallback.
                    client
                        .rollup_config()
                        .await
                        .map_err(|e| eyre::eyre!("optimism_rollupConfig probe failed: {e}"))?;
                    eyre::Ok(client)
                }
            })
            .await?;
        let rollup_config = Arc::new(
            rollup_client
                .rollup_config()
                .await
                .map_err(|e| eyre::eyre!("optimism_rollupConfig RPC failed: {e}"))?,
        );
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

        // Optionally block startup until the rollup node reports a non-zero
        // sync status. Mirrors the reference batcher's `--wait-node-sync`.
        if self.config.wait_node_sync {
            Self::wait_for_node_sync(
                &rollup_client,
                self.config.poll_interval,
                self.config.wait_node_sync_timeout,
            )
            .await?;
        }

        // Fetch sync status to determine the safe L2 head for startup backfill.
        let sync_status = rollup_client
            .sync_status()
            .await
            .map_err(|e| eyre::eyre!("optimism_syncStatus RPC failed: {e}"))?;
        let safe_l2_number = sync_status.safe_l2.block_info.number;
        let next_l2_timestamp =
            sync_status.safe_l2.block_info.timestamp.saturating_add(rollup_config.block_time);
        self.config.encoder_config.validate_for_rollup_config(&rollup_config, next_l2_timestamp)?;
        info!(safe_l2 = %safe_l2_number, "fetched safe L2 head");

        // Validate the recent-tx scan depth against the maximum. Do this early so
        // the error surfaces before any network I/O for the scan.
        if self.config.check_recent_txs_depth > MAX_CHECK_RECENT_TXS_DEPTH {
            return Err(eyre::eyre!(
                "check_recent_txs_depth {} exceeds maximum of {}",
                self.config.check_recent_txs_depth,
                MAX_CHECK_RECENT_TXS_DEPTH,
            ));
        }

        // Connect to L1 early so it is available for the optional recent-tx scan.
        let l1_provider: RootProvider =
            Self::connect_first(&self.config.l1_rpc_url, "l1-rpc", |url| {
                let url = url.clone();
                async move {
                    ProviderBuilder::new().disable_recommended_fillers().connect(url.as_str()).await
                }
            })
            .await?;

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

        if let Some(shadow_inbox) = self.config.batch_inbox_override {
            if shadow_inbox == rollup_config.batch_inbox_address {
                warn!(
                    inbox = %shadow_inbox,
                    "shadow parity monitor disabled because shadow inbox matches canonical inbox"
                );
            } else {
                let channel_timeout_depth = rollup_config.channel_timeout(next_l2_timestamp);
                let start_depth = self
                    .config
                    .check_recent_txs_depth
                    .max(channel_timeout_depth)
                    .clamp(1, MAX_CHECK_RECENT_TXS_DEPTH);
                let monitor_config = ShadowParityMonitorConfig {
                    canonical_inbox: rollup_config.batch_inbox_address,
                    canonical_batcher: rollup_config
                        .genesis
                        .system_config
                        .as_ref()
                        .map(|config| config.batcher_address),
                    shadow_inbox,
                    shadow_batcher: signer_address,
                    poll_interval: self.config.poll_interval,
                    start_depth,
                    rollup_config: Arc::clone(&rollup_config),
                    l1_beacon_url: self.config.l1_beacon_url.clone(),
                };
                match ShadowParityMonitor::new(l1_provider.clone(), monitor_config).await {
                    Ok(monitor) => {
                        let handle = monitor.spawn(cancellation.clone());
                        background_tasks.push(("shadow parity monitor", handle));
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "shadow parity monitor failed to initialize; continuing without parity monitoring"
                        );
                    }
                }
            }
        }

        if let Some(parity_validator_l2_rpc_url) = &self.config.parity_validator_l2_rpc_url {
            if self.config.batch_inbox_override.is_none() {
                eyre::bail!(
                    "parity validator L2 RPC URL requires shadow mode batch inbox override"
                );
            }

            match Self::connect_first(
                std::slice::from_ref(parity_validator_l2_rpc_url),
                "parity-validator-l2-rpc",
                |url| {
                    let url = url.clone();
                    async move {
                        let provider = ProviderBuilder::new()
                            .disable_recommended_fillers()
                            .network::<Base>()
                            .connect(url.as_str())
                            .await
                            .map_err(|e| {
                                eyre::eyre!("failed to connect parity validator L2 RPC: {e}")
                            })?;
                        provider.get_block_number().await.map_err(|e| {
                            eyre::eyre!("parity validator eth_blockNumber probe failed: {e}")
                        })?;
                        eyre::Ok(provider)
                    }
                },
            )
            .await
            {
                Ok(provider) => {
                    let validator_provider: Arc<dyn Provider<Base> + Send + Sync> =
                        Arc::new(provider);
                    let handle = L2BlockParityMonitor::new(
                        RpcL2BlockProvider::new(Arc::clone(&l2_provider)),
                        RpcL2BlockProvider::new(validator_provider),
                        L2BlockParityMonitorConfig::new(
                            safe_l2_number.saturating_add(1),
                            self.config.poll_interval,
                        ),
                    )
                    .spawn(cancellation.clone());
                    background_tasks.push(("derived L2 block parity monitor", handle));
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "derived L2 block parity monitor failed to connect; continuing without parity monitoring"
                    );
                }
            }
        }

        // Optionally scan recent L1 blocks to find the highest L2 block already
        // submitted but not yet reflected in the safe head, preventing re-submissions
        // after an unclean restart. Peek at the batcher address from the private key
        // (without consuming it) only when the scan is requested.
        let scanned_highest = if self.config.check_recent_txs_depth > 0 {
            let batcher_address = signer_address;
            RecentTxScanner::highest_submitted_l2_block(
                &l1_provider,
                batcher_address,
                effective_batch_inbox,
                self.config.check_recent_txs_depth,
                &rollup_config,
            )
            .await?
        } else {
            None
        };

        // Get the current L2 latest block to decide whether historical backfill is needed.
        let latest_l2 = l2_provider
            .get_block_number()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L2 latest block number: {e}"))?;

        // Advance the cursor past any L2 blocks that are already on L1 but not yet safe.
        // Use the higher of the safe head and the scan result as the backfill start.
        let cursor_start = safe_l2_number.max(scanned_highest.unwrap_or(0));

        // Build the L2 polling source. If blocks between cursor_start+1 and latest
        // were not yet submitted, use sequential catchup mode to avoid skipping them.
        let poller = if cursor_start < latest_l2 {
            info!(
                safe_l2 = %safe_l2_number,
                cursor_start = %cursor_start,
                latest_l2 = %latest_l2,
                "starting sequential backfill from cursor"
            );
            RpcPollingSource::new_from(Arc::clone(&l2_provider), cursor_start + 1)
        } else {
            RpcPollingSource::new(Arc::clone(&l2_provider))
        };

        // Assemble the hybrid L2 block source.
        let source = HybridBlockSource::new(
            TokioRuntime::new(),
            l2_subscription,
            poller,
            self.config.poll_interval,
        );
        let encoder =
            BatchEncoder::new(Arc::clone(&rollup_config), self.config.encoder_config.clone());

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
            Self::connect_first(&self.config.l1_rpc_url, "l1-rpc-poller", |url| {
                let url = url.clone();
                async move {
                    ProviderBuilder::new().disable_recommended_fillers().connect(url.as_str()).await
                }
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
        let l1_chain_id = l1_provider
            .get_chain_id()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch L1 chain ID: {e}"))?;
        let tx_manager_config = TxManagerConfig {
            resubmission_timeout: self.config.resubmission_timeout,
            num_confirmations: self.config.num_confirmations as u64,
            ..TxManagerConfig::default()
        };
        let tx_manager = SimpleTxManager::new(
            l1_provider,
            signer_config,
            tx_manager_config,
            l1_chain_id,
            Arc::new(BaseTxMetrics::new("batcher")),
        )
        .await
        .map_err(|e| eyre::eyre!("failed to create tx manager: {e}"))?;

        // Create a safe-head watch channel for runtime pruning of confirmed blocks.
        let (safe_head_tx, safe_head_rx) = watch::channel::<u64>(safe_l2_number);

        // Spawn the safe-head poller. It polls `optimism_syncStatus` at the
        // configured interval and advances the watch when the safe L2 head
        // moves forward, allowing the encoder to prune confirmed blocks.
        SafeHeadPoller::new(rollup_client, self.config.poll_interval, safe_head_tx)
            .spawn(runtime.clone());

        // Build the driver — all fallible setup is complete at this point.
        let mut driver = BatchDriver::new(
            runtime,
            encoder,
            source,
            tx_manager,
            base_batcher_core::BatchDriverConfig {
                inbox: effective_batch_inbox,
                max_pending_transactions: self.config.max_pending_transactions,
                drain_timeout: self.config.resubmission_timeout * 2,
                force_blobs_when_throttling: self.config.force_blobs_when_throttling,
            },
            DaThrottle::new(throttle, throttle_client),
            l1_head_source,
        )
        .with_safe_head_rx(safe_head_rx)
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
