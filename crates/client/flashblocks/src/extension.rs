//! Contains the [`FlashblocksExtension`] which wires up the flashblocks feature
//! (both the canon ExEx and RPC surface) on the Base node builder.

use std::sync::Arc;

use base_client_node::{BaseNodeExtension, OpBuilder, OpProvider};
use futures_util::TryStreamExt;
use once_cell::sync::OnceCell;
use reth_exex::ExExEvent;
use tracing::info;
use url::Url;

use crate::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksState,
    FlashblocksSubscriber,
};

/// The flashblocks cell holds a shared state reference.
pub type FlashblocksCell<T> = Arc<OnceCell<Arc<T>>>;

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: String,
    /// Maximum number of pending flashblocks to retain in memory.
    pub max_pending_blocks_depth: u64,
}

/// Helper struct that wires the Flashblocks feature (canon ExEx and RPC) into the node builder.
#[derive(Debug, Clone)]
pub struct FlashblocksExtension {
    /// Shared Flashblocks state cache.
    pub cell: FlashblocksCell<FlashblocksState<OpProvider>>,
    /// Optional Flashblocks configuration.
    pub config: Option<FlashblocksConfig>,
}

impl FlashblocksExtension {
    /// Create a new Flashblocks extension helper.
    pub const fn new(
        cell: FlashblocksCell<FlashblocksState<OpProvider>>,
        config: Option<FlashblocksConfig>,
    ) -> Self {
        Self { cell, config }
    }
}

impl BaseNodeExtension for FlashblocksExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let Some(cfg) = self.config else {
            info!(message = "flashblocks integration is disabled");
            return builder;
        };

        let flashblocks_cell = self.cell;
        let cfg_for_rpc = cfg.clone();
        let flashblocks_cell_for_rpc = flashblocks_cell.clone();

        // Install the canon ExEx
        let builder = builder.install_exex("flashblocks-canon", move |mut ctx| {
            let flashblocks_cell = flashblocks_cell.clone();
            async move {
                let fb = flashblocks_cell
                    .get_or_init(|| {
                        Arc::new(FlashblocksState::new(
                            ctx.provider().clone(),
                            cfg.max_pending_blocks_depth,
                        ))
                    })
                    .clone();

                Ok(async move {
                    while let Some(note) = ctx.notifications.try_next().await? {
                        if let Some(committed) = note.committed_chain() {
                            let tip = committed.tip().num_hash();
                            let chain = Arc::unwrap_or_clone(committed);
                            for (_, block) in chain.into_blocks() {
                                fb.on_canonical_block_received(block);
                            }
                            let _ = ctx.events.send(ExExEvent::FinishedHeight(tip));
                        }
                    }
                    Ok(())
                })
            }
        });

        // Extend with RPC modules
        builder.extend_rpc_modules(move |ctx| {
            info!(message = "Starting Flashblocks RPC");

            let ws_url = Url::parse(cfg_for_rpc.websocket_url.as_str())?;
            let fb = flashblocks_cell_for_rpc
                .get_or_init(|| {
                    Arc::new(FlashblocksState::new(
                        ctx.provider().clone(),
                        cfg_for_rpc.max_pending_blocks_depth,
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
