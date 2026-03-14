//! Batcher service startup and wiring.

use std::sync::Arc;

use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_alloy_consensus::OpBlock;
use base_alloy_network::Base;
use base_batcher_core::{BatchDriver, ThrottleConfig, ThrottleController, ThrottleStrategy};
use base_batcher_encoder::BatchEncoder;
use base_batcher_source::{BlockSubscription, HybridBlockSource, SourceError};
use base_consensus_genesis::RollupConfig;
use futures::{StreamExt, stream::BoxStream};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    BatcherConfig, NoopTxManager, NullSubscription, RpcPollingSource, WsBlockSubscription,
};

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

/// The batcher service.
///
/// Wires the encoder, block source, transaction manager, and driver
/// into a running batcher process. Call [`start`](Self::start) to run.
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

    /// Build a block subscription for the given L2 RPC URL.
    ///
    /// For `ws://` or `wss://` URLs, connects a dedicated WS provider, subscribes
    /// to new block headers, and builds a stream that fetches the full block for
    /// each header. The provider is wrapped in a [`WsBlockSubscription`] so its
    /// lifetime is tied to the returned subscription — and therefore to the
    /// [`HybridBlockSource`] that consumes it — rather than to this function's
    /// stack frame.
    ///
    /// For non-WS URLs, returns a [`NullSubscription`] so that
    /// [`HybridBlockSource`] falls back entirely to polling.
    ///
    /// [`HybridBlockSource`]: base_batcher_source::HybridBlockSource
    async fn build_subscription(
        url: &Url,
        fetch_provider: Arc<dyn Provider<Base> + Send + Sync>,
    ) -> Subscription {
        if !matches!(url.scheme(), "ws" | "wss") {
            warn!(l2_rpc = %url, "L2 RPC is not a WebSocket URL; using polling only");
            return Subscription::Null(NullSubscription);
        }

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

    /// Start the batcher service.
    ///
    /// Constructs the encoding pipeline, block source, and driver, then
    /// runs the driver loop until the cancellation token is triggered.
    pub async fn start(self, cancellation: CancellationToken) -> eyre::Result<()> {
        info!(
            l1_rpc = %self.config.l1_rpc_url,
            l2_rpc = %self.config.l2_rpc_url,
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

        // Build a block subscription. For WS/WSS URLs the subscription owns its
        // provider Arc, so the connection stays live for the full driver run.
        // For non-WS URLs a NullSubscription is returned and HybridBlockSource
        // relies entirely on the polling path.
        let subscription =
            Self::build_subscription(&self.config.l2_rpc_url, Arc::clone(&l2_provider)).await;

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

        // Build the throttle controller.
        let (throttle_config, throttle_strategy) = self.config.throttle.clone().map_or(
            (
                ThrottleConfig { threshold_bytes: u64::MAX, max_intensity: 0.0 },
                ThrottleStrategy::Off,
            ),
            |cfg| (cfg, ThrottleStrategy::Linear),
        );
        let throttle = ThrottleController::new(throttle_config, throttle_strategy);

        // TODO: Wire in SimpleTxManager with L1 provider, wallet, and chain ID.
        let tx_manager = NoopTxManager;

        // Build and run the driver.
        let driver = BatchDriver::new(
            encoder,
            source,
            tx_manager,
            rollup_config.batch_inbox_address,
            self.config.max_pending_transactions,
            throttle,
            cancellation,
        );

        info!("batcher service components initialized");
        driver.run().await?;

        info!("batcher service shutting down");
        Ok(())
    }
}
