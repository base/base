//! Contains the [BaseRpcExtension] which wires up the custom Base RPC modules on the node builder.

use std::sync::Arc;

use base_reth_flashblocks::{FlashblocksState, FlashblocksSubscriber};
use base_reth_rpc::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, MeteringApiImpl,
    MeteringApiServer, TransactionStatusApiImpl, TransactionStatusApiServer,
};
use tracing::info;
use url::Url;

use crate::{
    BaseNodeConfig, FlashblocksConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, OpBuilder},
};

/// Helper struct that wires the custom RPC modules into the node builder.
#[derive(Debug, Clone)]
pub struct BaseRpcExtension {
    /// Shared Flashblocks state cache.
    pub flashblocks_cell: FlashblocksCell,
    /// Optional Flashblocks configuration.
    pub flashblocks: Option<FlashblocksConfig>,
    /// Indicates whether the metering RPC surface should be installed.
    pub metering_enabled: bool,
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
}

impl BaseRpcExtension {
    /// Creates a new RPC extension helper.
    pub fn new(config: &BaseNodeConfig) -> Self {
        Self {
            flashblocks_cell: config.flashblocks_cell.clone(),
            flashblocks: config.flashblocks.clone(),
            metering_enabled: config.metering_enabled,
            sequencer_rpc: config.rollup_args.sequencer.clone(),
        }
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

impl ConfigurableBaseNodeExtension for BaseRpcExtension {
    fn build(config: &BaseNodeConfig) -> eyre::Result<Self> {
        Ok(Self::new(config))
    }
}
