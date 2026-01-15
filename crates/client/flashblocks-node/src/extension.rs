//! Contains the [`FlashblocksExtension`] which wires up the flashblocks feature
//! (canonical block subscription and RPC surface) on the Base node builder.

use base_client_node::{BaseBuilder, BaseNodeExtension, FromExtensionConfig};
use base_flashblocks::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksConfig,
    FlashblocksSubscriber,
};
use reth_chain_state::CanonStateSubscriptions;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::info;

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
    fn apply(self: Box<Self>, builder: BaseBuilder) -> BaseBuilder {
        let Some(cfg) = self.config else {
            info!(message = "flashblocks integration is disabled");
            return builder;
        };

        let state = cfg.state;
        let mut subscriber = FlashblocksSubscriber::new(state.clone(), cfg.websocket_url);

        let state_for_canonical = state.clone();
        let state_for_rpc = state.clone();
        let state_for_start = state;

        // Start state processor, subscriber, and canonical subscription after node is started
        let builder = builder.add_node_started_hook(move |ctx| {
            info!(message = "Starting Flashblocks state processor");
            state_for_start.start(ctx.provider().clone());
            subscriber.start();

            let mut canonical_stream =
                BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
            tokio::spawn(async move {
                while let Some(Ok(notification)) = canonical_stream.next().await {
                    let committed = notification.committed();
                    for block in committed.blocks_iter() {
                        state_for_canonical.on_canonical_block_received(block.clone());
                    }
                }
            });

            Ok(())
        });

        // Extend with RPC modules
        builder.add_rpc_module(move |ctx| {
            info!(message = "Starting Flashblocks RPC");

            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                state_for_rpc.clone(),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;

            // Register the eth_subscribe subscription endpoint for flashblocks
            // Uses replace_configured since eth_subscribe already exists from reth's standard module
            // Pass eth_api to enable proxying standard subscription types to reth's implementation
            let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), state_for_rpc);
            ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

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
