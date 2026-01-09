//! Contains the [`FlashblocksRpcExtension`] which wires up the flashblocks RPC surface
//! on the Base node builder.

use std::sync::Arc;

use base_primitives::{
    BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, FlashblocksConfig,
    OpBuilder, OpProvider,
};
use tracing::info;
use url::Url;

use crate::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksState,
    FlashblocksSubscriber,
};

/// Helper struct that wires the flashblocks RPC into the node builder.
#[derive(Debug, Clone)]
pub struct FlashblocksRpcExtension {
    /// Shared Flashblocks state cache.
    pub cell: FlashblocksCell<FlashblocksState<OpProvider>>,
    /// Optional Flashblocks configuration.
    pub config: Option<FlashblocksConfig>,
}

impl FlashblocksRpcExtension {
    /// Creates a new flashblocks RPC extension.
    pub const fn new(
        cell: FlashblocksCell<FlashblocksState<OpProvider>>,
        config: Option<FlashblocksConfig>,
    ) -> Self {
        Self { cell, config }
    }
}

impl BaseNodeExtension for FlashblocksRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let Some(cfg) = self.config else {
            info!(message = "flashblocks RPC integration is disabled");
            return builder;
        };

        let flashblocks_cell = self.cell;

        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Flashblocks RPC");

            let ws_url = Url::parse(cfg.websocket_url.as_str())?;
            let fb = flashblocks_cell
                .get_or_init(|| {
                    Arc::new(FlashblocksState::new(
                        ctx.provider().clone(),
                        cfg.max_pending_blocks_depth,
                    ))
                })
                .clone();
            fb.start();

            let mut flashblocks_client = FlashblocksSubscriber::new(fb.clone(), ws_url);
            flashblocks_client.start();

            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                fb.clone(),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;

            // Register the eth_subscribe subscription endpoint for flashblocks
            // Uses replace_configured since eth_subscribe already exists from reth's standard module
            // Pass eth_api to enable proxying standard subscription types to reth's implementation
            let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), fb);
            ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

            Ok(())
        })
    }
}

// Reuse FlashblocksCanonConfig since the methods are identical.
// Any type implementing FlashblocksCanonConfig can construct a FlashblocksRpcExtension.
impl<C: crate::FlashblocksCanonConfig> ConfigurableBaseNodeExtension<C>
    for FlashblocksRpcExtension
{
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(config.flashblocks_cell().clone(), config.flashblocks().cloned()))
    }
}
