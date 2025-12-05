//! Contains the [BaseRpcExtension] which wires up the custom Base RPC modules on the node builder.

use std::sync::Arc;

use base_reth_flashblocks_rpc::{
    pubsub::{BasePubSub, BasePubSubApiServer},
    rpc::{EthApiExt, EthApiOverrideServer},
    state::FlashblocksState,
    subscription::FlashblocksSubscriber,
};
use base_reth_metering::{MeteringApiImpl, MeteringApiServer};
use base_reth_transaction_status::{TransactionStatusApiImpl, TransactionStatusApiServer};
use tracing::info;
use url::Url;

use crate::{
    FlashblocksConfig,
    extensions::{FlashblocksCell, OpBuilder},
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
    pub const fn new(
        flashblocks_cell: FlashblocksCell,
        flashblocks: Option<FlashblocksConfig>,
        metering_enabled: bool,
        sequencer_rpc: Option<String>,
    ) -> Self {
        Self { flashblocks_cell, flashblocks, metering_enabled, sequencer_rpc }
    }

    /// Applies the extension to the supplied builder.
    pub fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let flashblocks_cell = self.flashblocks_cell.clone();
        let flashblocks = self.flashblocks.clone();
        let metering_enabled = self.metering_enabled;
        let sequencer_rpc = self.sequencer_rpc.clone();

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

                // Register the base_subscribe subscription endpoint
                let base_pubsub = BasePubSub::new(fb);
                ctx.modules.merge_configured(base_pubsub.into_rpc())?;
            } else {
                info!(message = "flashblocks integration is disabled");
            }

            Ok(())
        })
    }
}
