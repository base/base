//! Contains the [`FlashblocksExtension`] which wires up the flashblocks feature
//! (canonical block subscription and RPC surface) on the Base node builder.

use std::sync::Arc;

use base_client_node::{BaseNodeExtension, FromExtensionConfig, OpBuilder};
use reth_chain_state::CanonStateSubscriptions;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::info;
use url::Url;

use crate::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksState,
    FlashblocksSubscriber,
};

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// Maximum number of pending flashblocks to retain in memory.
    pub max_pending_blocks_depth: u64,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration.
    pub fn new(websocket_url: String, max_pending_blocks_depth: u64) -> Self {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));
        let ws_url = Url::parse(&websocket_url).expect("valid websocket URL");
        Self { websocket_url: ws_url, max_pending_blocks_depth, state }
    }
}

/// Helper struct that wires the Flashblocks feature (canonical subscription and RPC) into the node builder.
#[derive(Debug)]
pub struct FlashblocksExtension {
    /// Optional Flashblocks configuration (includes state).
    config: Option<FlashblocksConfig>,
}

impl FlashblocksExtension {
    /// Create a new Flashblocks extension helper.
    pub const fn new(config: Option<FlashblocksConfig>) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for FlashblocksExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let Some(cfg) = self.config else {
            info!(message = "flashblocks integration is disabled");
            return builder;
        };

        let state = cfg.state;
        let mut subscriber = FlashblocksSubscriber::new(state.clone(), cfg.websocket_url);

        let state_for_rpc = state.clone();
        let state_for_start = state;

        // Start state processor and subscriber after node is started
        let builder = builder.on_node_started(move |ctx| {
            info!(message = "Starting Flashblocks state processor");
            state_for_start.start(ctx.provider().clone());
            subscriber.start();
            Ok(())
        });

        // Extend with RPC modules and start canonical block subscription
        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Flashblocks RPC");

            let fb = state_for_rpc;

            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                fb.clone(),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;

            // Register the eth_subscribe subscription endpoint for flashblocks
            // Uses replace_configured since eth_subscribe already exists from reth's standard module
            // Pass eth_api to enable proxying standard subscription types to reth's implementation
            let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), fb.clone());
            ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

            let mut canonical_stream =
                BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
            tokio::spawn(async move {
                while let Some(Ok(notification)) = canonical_stream.next().await {
                    let committed = notification.committed();
                    for block in committed.blocks_iter() {
                        fb.on_canonical_block_received(block.clone());
                    }
                }
            });

            Ok(())
        })
    }
}

impl FromExtensionConfig for FlashblocksExtension {
    type Config = Option<FlashblocksConfig>;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}
