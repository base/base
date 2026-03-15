//! Batcher service startup and wiring.

use std::sync::Arc;

use alloy_network::EthereumWallet;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use base_alloy_consensus::OpBlock;
use base_alloy_network::Base;
use base_batcher_core::{
    BatchDriver, NoopThrottleClient, ThrottleClient, ThrottleConfig, ThrottleController,
    ThrottleStrategy,
};
use base_batcher_encoder::BatchEncoder;
use base_batcher_source::{BlockSubscription, HybridBlockSource, SourceError};
use base_consensus_genesis::RollupConfig;
use base_tx_manager::{NoopTxMetrics, SimpleTxManager, TxManagerConfig};
use futures::{StreamExt, future::BoxFuture, stream::BoxStream};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    BatcherConfig, NullSubscription, RpcPollingSource, RpcThrottleClient, WsBlockSubscription,
};

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

/// Batcher-internal subscription variant: either a live WS subscription or a no-op.
///
/// Using a concrete enum avoids heap allocation while still allowing
/// `build_subscription` to return either branch to `start`.
enum Subscription {
    Ws(WsBlockSubscription),
    Null(NullSubscription),
}

impl BlockSubscription for Subscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<OpBlock, SourceError>> {
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
    BatchEncoder,
    HybridBlockSource<Subscription, RpcPollingSource>,
    SimpleTxManager,
    ServiceThrottle,
>;

/// A fully-initialised batcher ready to run the submission loop.
///
/// Created by [`BatcherService::setup`]. All connections are live and the
/// rollup config has been fetched. Call [`run`](Self::run) to enter the
/// main driver loop, or spawn it in a background task for in-process use.
pub struct ReadyBatcher {
    driver: ServiceDriver,
}

impl std::fmt::Debug for ReadyBatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyBatcher").finish_non_exhaustive()
    }
}

impl ReadyBatcher {
    /// Run the batch submission loop until `cancellation` is triggered.
    pub async fn run(self, cancellation: CancellationToken) -> eyre::Result<()> {
        info!("batcher driver running");
        self.driver.run(cancellation).await?;
        info!("batcher service shutting down");
        Ok(())
    }
}

/// The batcher service.
///
/// Wires the encoder, block source, transaction manager, and driver.
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
    async fn build_subscription(
        url: Option<&Url>,
        fetch_provider: Arc<dyn Provider<Base> + Send + Sync>,
    ) -> Subscription {
        let Some(url) = url else {
            return Subscription::Null(NullSubscription);
        };

        let ws_provider = match ProviderBuilder::new().connect(url.as_str()).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn!(error = %e, l2_rpc = %url, "failed to connect WS provider; falling back to polling");
                return Subscription::Null(NullSubscription);
            }
        };

        let sub = match ws_provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to subscribe to new blocks; falling back to polling");
                return Subscription::Null(NullSubscription);
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
                    let block =
                        rpc_block.into_consensus().map_transactions(|t| t.inner.into_inner());
                    Ok(block)
                }
            })
            .boxed();

        Subscription::Ws(WsBlockSubscription::new(ws_provider, stream))
    }

    /// Initialise all batcher components and return a [`ReadyBatcher`].
    ///
    /// Connects to the L2 and L1 RPC endpoints, fetches the rollup config,
    /// validates the private key, and constructs the driver. Returns an error
    /// if any of those steps fail — the caller sees the failure immediately,
    /// before any background work is spawned.
    pub async fn setup(self) -> eyre::Result<ReadyBatcher> {
        info!(
            l1_rpc = %self.config.l1_rpc_url,
            l2_rpc = %self.config.l2_rpc_url,
            l2_ws = self.config.l2_ws_url.as_ref().map(|u| u.as_str()),
            "starting batcher service"
        );

        // Connect to the L2 RPC endpoint.
        let l2_provider: Arc<dyn Provider<Base> + Send + Sync> = Arc::new(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<Base>()
                .connect(self.config.l2_rpc_url.as_str())
                .await?,
        );

        // Build the polling source.
        let poller = RpcPollingSource::new(Arc::clone(&l2_provider));

        // Build a block subscription. When l2_ws_url is configured the
        // subscription owns its provider Arc so the connection stays live for
        // the full driver run. Without a WS URL a NullSubscription is returned
        // and HybridBlockSource relies entirely on the polling path.
        let subscription =
            Self::build_subscription(self.config.l2_ws_url.as_ref(), Arc::clone(&l2_provider))
                .await;

        // Assemble the hybrid block source.
        let source = HybridBlockSource::new(subscription, poller, self.config.poll_interval);

        // Fetch the rollup config from the rollup node via `optimism_rollupConfig`.
        // Uses a plain HTTP provider so no network-typed provider is needed — the
        // rollup node endpoint is different from the L2 execution node.
        let rollup_config = {
            let rollup_provider: RootProvider = ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect(self.config.rollup_rpc_url.as_str())
                .await
                .map_err(|e| eyre::eyre!("failed to connect to rollup node: {e}"))?;
            info!(rollup_rpc = %self.config.rollup_rpc_url, "fetching rollup config");
            let raw: Value = rollup_provider
                .raw_request("optimism_rollupConfig".into(), ())
                .await
                .map_err(|e| eyre::eyre!("optimism_rollupConfig RPC failed: {e}"))?;
            Arc::new(
                serde_json::from_value::<RollupConfig>(raw)
                    .map_err(|e| eyre::eyre!("failed to deserialize rollup config: {e}"))?,
            )
        };
        info!(
            inbox = %rollup_config.batch_inbox_address,
            "rollup config loaded"
        );
        let encoder =
            BatchEncoder::new(Arc::clone(&rollup_config), self.config.encoder_config.clone());

        // Build the throttle controller and the appropriate client.
        // When throttling is disabled we use a NoopThrottleClient so the driver
        // never calls miner_setMaxDASize on the sequencer.
        let throttle_client = match &self.config.throttle {
            None => ServiceThrottle::Noop(NoopThrottleClient),
            Some(_) => {
                ServiceThrottle::Rpc(RpcThrottleClient::new(self.config.l2_rpc_url.as_str())?)
            }
        };
        let (throttle_config, throttle_strategy) = self.config.throttle.clone().map_or_else(
            || (ThrottleConfig::default(), ThrottleStrategy::Off),
            |cfg| (cfg, ThrottleStrategy::Linear),
        );
        let throttle = ThrottleController::new(throttle_config, throttle_strategy);

        // Connect to the L1 RPC endpoint for transaction submission.
        let l1_provider: RootProvider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect(self.config.l1_rpc_url.as_str())
            .await
            .map_err(|e| eyre::eyre!("failed to connect to L1: {e}"))?;

        // Build the batcher wallet from the configured private key.
        let signer = PrivateKeySigner::from_bytes(&self.config.batcher_private_key.0)
            .map_err(|e| eyre::eyre!("invalid batcher private key: {e}"))?;
        let wallet = EthereumWallet::new(signer);

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
            wallet,
            tx_manager_config,
            l1_chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .map_err(|e| eyre::eyre!("failed to create tx manager: {e}"))?;

        // Build the driver — all fallible setup is complete at this point.
        let driver = BatchDriver::new(
            encoder,
            source,
            tx_manager,
            rollup_config.batch_inbox_address,
            self.config.max_pending_transactions,
            throttle,
            throttle_client,
        );

        info!("batcher service components initialized");
        Ok(ReadyBatcher { driver })
    }
}
