use std::sync::Arc;

use base_reth_flashblocks_rpc::{
    rpc::{EthApiExt, EthApiOverrideServer},
    state::FlashblocksState,
    subscription::FlashblocksSubscriber,
};
use base_reth_flashblocks_rpc::pubsub::{BasePubSub, BasePubSubApiServer};
use base_reth_metering::{MeteringApiImpl, MeteringApiServer};
use base_reth_transaction_status::{TransactionStatusApiImpl, TransactionStatusApiServer};
use base_reth_transaction_tracing::transaction_tracing_exex;
use eyre::Result;
use futures_util::TryStreamExt;
use once_cell::sync::OnceCell;
use reth::{
    builder::{EngineNodeLauncher, Node, NodeHandle, TreeConfig},
    providers::providers::BlockchainProvider,
};
use reth_exex::ExExEvent;
use reth_optimism_node::OpNode;
use tracing::info;
use url::Url;

use crate::{BaseNodeBuilder, BaseNodeConfig};

/// Wraps the Base node configuration and orchestrates builder wiring.
#[derive(Debug, Clone)]
pub struct BaseNodeLauncher {
    config: BaseNodeConfig,
}

impl BaseNodeLauncher {
    /// Creates a new launcher using the provided configuration.
    pub fn new(config: impl Into<BaseNodeConfig>) -> Self {
        Self { config: config.into() }
    }

    /// Returns the underlying configuration, primarily for testing.
    pub const fn config(&self) -> &BaseNodeConfig {
        &self.config
    }

    /// Applies all Base-specific wiring to the supplied builder and launches the node.
    pub async fn build_and_run(&self, builder: BaseNodeBuilder) -> Result<()> {
        info!(message = "starting custom Base node");

        let op_node = OpNode::new(self.config.rollup_args.clone());

        let flashblocks_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());

        let builder = builder
            .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
            .with_components(op_node.components())
            .with_add_ons(op_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        let tracing = self.config.tracing;
        let flashblocks = self.config.flashblocks.clone();
        let builder = builder
            .install_exex_if(tracing.enabled, "transaction-tracing", move |ctx| async move {
                Ok(transaction_tracing_exex(ctx, tracing.logs_enabled))
            })
            .install_exex_if(flashblocks.is_some(), "flashblocks-canon", {
                let flashblocks_cell = flashblocks_cell.clone();
                move |mut ctx| {
                    let flashblocks_cell = flashblocks_cell.clone();
                    let flashblocks = flashblocks;
                    async move {
                        let fb_config = flashblocks.expect("flashblocks config checked above");
                        let fb = flashblocks_cell
                            .get_or_init(|| {
                                Arc::new(FlashblocksState::new(
                                    ctx.provider().clone(),
                                    fb_config.max_pending_blocks_depth,
                                ))
                            })
                            .clone();

                        Ok(async move {
                            while let Some(note) = ctx.notifications.try_next().await? {
                                if let Some(committed) = note.committed_chain() {
                                    for block in committed.blocks_iter() {
                                        fb.on_canonical_block_received(block);
                                    }
                                    let _ = ctx.events.send(ExExEvent::FinishedHeight(
                                        committed.tip().num_hash(),
                                    ));
                                }
                            }
                            Ok(())
                        })
                    }
                }
            });

        let metering_enabled = self.config.metering_enabled;
        let sequencer_rpc = self.config.rollup_args.sequencer.clone();
        let builder = builder.extend_rpc_modules({
            let flashblocks_cell = flashblocks_cell.clone();
            let flashblocks = self.config.flashblocks.clone();
            move |ctx| {
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
            }
        });

        let NodeHandle { node: _, node_exit_future } = builder
            .launch_with_fn(|builder| {
                let engine_tree_config = TreeConfig::default()
                    .with_persistence_threshold(builder.config().engine.persistence_threshold)
                    .with_memory_block_buffer_target(
                        builder.config().engine.memory_block_buffer_target,
                    );

                let launcher = EngineNodeLauncher::new(
                    builder.task_executor().clone(),
                    builder.config().datadir(),
                    engine_tree_config,
                );

                builder.launch_with(launcher)
            })
            .await?;

        node_exit_future.await
    }
}
