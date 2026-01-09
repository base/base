//! Contains the [BaseRpcExtension] which wires up the custom Base RPC modules on the node builder.

use std::sync::Arc;

use base_primitives::{
    BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, FlashblocksConfig,
    OpBuilder, OpProvider,
};
use base_reth_flashblocks::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksState,
    FlashblocksSubscriber,
};
use base_reth_metering::{MeteringApiImpl, MeteringApiServer};
use base_txpool::{TransactionStatusApiImpl, TransactionStatusApiServer};
use tracing::info;
use url::Url;

/// Helper struct that wires the custom RPC modules into the node builder.
#[derive(Debug, Clone)]
pub struct BaseRpcExtension {
    /// Shared Flashblocks state cache.
    pub flashblocks_cell: FlashblocksCell<FlashblocksState<OpProvider>>,
    /// Optional Flashblocks configuration.
    pub flashblocks: Option<FlashblocksConfig>,
    /// Indicates whether the metering RPC surface should be installed.
    pub metering_enabled: bool,
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
}

impl BaseRpcExtension {
    /// Creates a new RPC extension helper.
    pub const fn new(
        flashblocks_cell: FlashblocksCell<FlashblocksState<OpProvider>>,
        flashblocks: Option<FlashblocksConfig>,
        metering_enabled: bool,
        sequencer_rpc: Option<String>,
    ) -> Self {
        Self { flashblocks_cell, flashblocks, metering_enabled, sequencer_rpc }
    }
}

impl BaseNodeExtension for BaseRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let flashblocks_cell = self.flashblocks_cell;
        let flashblocks = self.flashblocks;
        let metering_enabled = self.metering_enabled;
        let sequencer_rpc = self.sequencer_rpc;

        builder.extend_rpc_modules(move |ctx| {
            if metering_enabled {
                info!(message = "Starting Metering RPC");
                let metering_api = MeteringApiImpl::new(ctx.provider().clone());
                ctx.modules.merge_configured(metering_api.into_rpc())?;
            }

            let proxy_api =
                TransactionStatusApiImpl::new(sequencer_rpc.clone(), ctx.pool().clone())
                    .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;

            if let Some(cfg) = flashblocks.clone() {
                info!(message = "Starting Flashblocks");

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
            } else {
                info!(message = "flashblocks integration is disabled");
            }

            Ok(())
        })
    }
}

/// Configuration trait for [`BaseRpcExtension`].
///
/// Types implementing this trait can be used to construct a [`BaseRpcExtension`].
pub trait BaseRpcConfig {
    /// Returns the shared flashblocks cell.
    fn flashblocks_cell(&self) -> &FlashblocksCell<FlashblocksState<OpProvider>>;
    /// Returns the flashblocks configuration if enabled.
    fn flashblocks(&self) -> Option<&FlashblocksConfig>;
    /// Returns whether metering is enabled.
    fn metering_enabled(&self) -> bool;
    /// Returns the sequencer RPC URL if configured.
    fn sequencer_rpc(&self) -> Option<&str>;
}

impl<C: BaseRpcConfig> ConfigurableBaseNodeExtension<C> for BaseRpcExtension {
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(
            config.flashblocks_cell().clone(),
            config.flashblocks().cloned(),
            config.metering_enabled(),
            config.sequencer_rpc().map(String::from),
        ))
    }
}
