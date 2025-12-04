//! Contains the node building and running logic

use std::sync::Arc;

use base_reth_flashblocks_rpc::{
    rpc::{EthApiExt, EthApiOverrideServer},
    state::FlashblocksState,
    subscription::FlashblocksSubscriber,
};
use base_reth_metering::{MeteringApiImpl, MeteringApiServer};
use base_reth_transaction_status::{TransactionStatusApiImpl, TransactionStatusApiServer};
use base_reth_transaction_tracing::transaction_tracing_exex;
use eyre::Result;
use futures_util::TryStreamExt;
use once_cell::sync::OnceCell;
use reth::{
    builder::{EngineNodeLauncher, Node, NodeBuilder, NodeHandle, TreeConfig, WithLaunchContext},
    providers::providers::BlockchainProvider,
};
use reth_db::DatabaseEnv;
use reth_exex::ExExEvent;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::OpNode;
use tracing::info;
use url::Url;

use crate::cli::Args;

type BaseNodeBuilder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>;

/// Builds a Base flavoured Reth node with every feature toggle derived from the CLI args
/// and runs it until shutdown.
pub async fn build_and_run_node(builder: BaseNodeBuilder, args: Args) -> Result<()> {
    info!(message = "starting custom Base node");

    let Args {
        rollup_args,
        websocket_url,
        max_pending_blocks_depth,
        enable_transaction_tracing,
        enable_transaction_tracing_logs,
        enable_metering,
    } = args;

    let flashblocks_enabled = websocket_url.is_some();
    let sequencer_rpc = rollup_args.sequencer.clone();
    let op_node = OpNode::new(rollup_args);
    let fb_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());

    // 1. Build the base node components.
    let builder = builder
        .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
        .with_components(op_node.components())
        .with_add_ons(op_node.add_ons())
        .on_component_initialized(move |_ctx| Ok(()));

    // 2. Wire up optional ExEx tasks.
    let builder = builder
        .install_exex_if(enable_transaction_tracing, "transaction-tracing", move |ctx| async move {
            Ok(transaction_tracing_exex(ctx, enable_transaction_tracing_logs))
        })
        .install_exex_if(flashblocks_enabled, "flashblocks-canon", {
            let fb_cell = fb_cell.clone();
            move |mut ctx| {
                let fb_cell = fb_cell.clone();
                async move {
                    let fb = fb_cell
                        .get_or_init(|| {
                            Arc::new(FlashblocksState::new(
                                ctx.provider().clone(),
                                max_pending_blocks_depth,
                            ))
                        })
                        .clone();

                    Ok(async move {
                        while let Some(note) = ctx.notifications.try_next().await? {
                            if let Some(committed) = note.committed_chain() {
                                for block in committed.blocks_iter() {
                                    fb.on_canonical_block_received(block);
                                }
                                let _ = ctx
                                    .events
                                    .send(ExExEvent::FinishedHeight(committed.tip().num_hash()));
                            }
                        }
                        Ok(())
                    })
                }
            }
        });

    // 3. Extend the RPC surface with Base specific modules.
    let builder = builder.extend_rpc_modules({
        let websocket_url = websocket_url.clone();
        let fb_cell = fb_cell.clone();
        let sequencer_rpc = sequencer_rpc.clone();
        move |ctx| {
            if enable_metering {
                info!(message = "Starting Metering RPC");
                let metering_api = MeteringApiImpl::new(ctx.provider().clone());
                ctx.modules.merge_configured(metering_api.into_rpc())?;
            }

            let proxy_api =
                TransactionStatusApiImpl::new(sequencer_rpc.clone(), ctx.pool().clone())
                    .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;

            if flashblocks_enabled {
                info!(message = "Starting Flashblocks");

                let ws_url = Url::parse(
                    websocket_url
                        .as_ref()
                        .expect("WEBSOCKET_URL must be set when Flashblocks is enabled")
                        .as_str(),
                )?;

                let fb = fb_cell
                    .get_or_init(|| {
                        Arc::new(FlashblocksState::new(
                            ctx.provider().clone(),
                            max_pending_blocks_depth,
                        ))
                    })
                    .clone();
                fb.start();

                let mut flashblocks_client = FlashblocksSubscriber::new(fb.clone(), ws_url);
                flashblocks_client.start();

                let api_ext = EthApiExt::new(
                    ctx.registry.eth_api().clone(),
                    ctx.registry.eth_handlers().filter.clone(),
                    fb,
                );

                ctx.modules.replace_configured(api_ext.into_rpc())?;
            } else {
                info!(message = "flashblocks integration is disabled");
            }

            Ok(())
        }
    });

    // 4. Launch the node with the bespoke Engine tree settings.
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
