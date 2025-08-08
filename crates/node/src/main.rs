use base_reth_flashblocks_rpc::rpc::EthApiExt;
use futures_util::TryStreamExt;
use once_cell::sync::OnceCell;
use reth_exex::ExExEvent;
use std::sync::Arc;

use base_reth_flashblocks_rpc::rpc::EthApiOverrideServer;
use base_reth_flashblocks_rpc::state::FlashblocksState;
use base_reth_flashblocks_rpc::subscription::FlashblocksSubscriber;
use clap::Parser;
use reth::builder::Node;
use reth::{
    builder::{EngineNodeLauncher, TreeConfig},
    providers::providers::BlockchainProvider,
};
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::args::RollupArgs;
use reth_optimism_node::OpNode;
use tracing::info;
use url::Url;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
struct FlashblocksRollupArgs {
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,
}

impl FlashblocksRollupArgs {
    fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }
}

fn main() {
    Cli::<OpChainSpecParser, FlashblocksRollupArgs>::parse()
        .run(|builder, flashblocks_rollup_args| async move {
            info!(message = "starting custom Base node");

            let flashblocks_enabled = flashblocks_rollup_args.flashblocks_enabled();
            let ws_url_opt = flashblocks_rollup_args.websocket_url.clone();
            let op_node = OpNode::new(flashblocks_rollup_args.rollup_args.clone());

            // Shared cell for a single FlashblocksState instance
            let fb_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());

            let handle = builder
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                // ExEx: canonical listener clears pending when canon ≥ pending
                .install_exex_if(flashblocks_enabled, "flashblocks-canon", {
                    let fb_cell = fb_cell.clone();
                    move |mut ctx| async move {
                        // Initialize or reuse the shared state (created with ctx.provider())
                        let fb = fb_cell
                            .get_or_init(|| Arc::new(FlashblocksState::new(ctx.provider().clone())))
                            .clone();

                        // Return the async task (do NOT await it here)
                        Ok(async move {
                            // Handle ExEx notifications (TryStream version)
                            while let Some(note) = ctx.notifications.try_next().await? {
                                if let Some(committed) = note.committed_chain() {
                                    for b in committed.blocks_iter() {
                                        fb.clear_on_canonical_catchup(b.number);
                                    }
                                    let _ = ctx.events.send(ExExEvent::FinishedHeight(
                                        committed.tip().num_hash(),
                                    ));
                                }
                            }
                            Ok(())
                        })
                    }
                })
                // RPC: start subscriber and expose APIs, using the *same* shared state
                .extend_rpc_modules({
                    let fb_cell = fb_cell.clone();
                    move |ctx| {
                        if flashblocks_enabled {
                            let ws_url =
                                Url::parse(ws_url_opt.as_ref().expect(
                                    "WEBSOCKET_URL must be set when Flashblocks is enabled",
                                ))?;

                            // Reuse/init the same instance the ExEx listener uses
                            let fb = fb_cell
                                .get_or_init(|| {
                                    Arc::new(FlashblocksState::new(ctx.provider().clone()))
                                })
                                .clone();

                            // Start WS subscriber feeding flashblocks into state
                            let mut sub = FlashblocksSubscriber::new(fb.clone(), ws_url);
                            sub.start();

                            // Install extended RPC backed by the same state
                            let api_ext = EthApiExt::new(ctx.registry.eth_api().clone(), fb);
                            ctx.modules.replace_configured(api_ext.into_rpc())?;
                        } else {
                            info!(message = "flashblocks integration is disabled");
                        }
                        Ok(())
                    }
                })
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

            handle.wait_for_node_exit().await
        })
        .unwrap();
}
