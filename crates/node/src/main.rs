use base_reth_flashblocks_rpc::{cache::Cache, flashblocks::FlashblocksClient, rpc::EthApiExt};
use std::sync::Arc;
use std::time::Duration;

use base_reth_flashblocks_rpc::rpc::EthApiOverrideServer;
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

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
struct FlashblocksRollupArgs {
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: String,
}

fn main() {
    Cli::<OpChainSpecParser, FlashblocksRollupArgs>::parse()
        .run(|builder, flashblocks_rollup_args| async move {
            info!("Starting custom Base node");
            let cache = Arc::new(Cache::new());
            let op_node = OpNode::new(flashblocks_rollup_args.rollup_args.clone());
            let mut flashblocks_client = FlashblocksClient::new(Arc::clone(&cache));

            let cache_clone = Arc::clone(&cache);
            let chain_spec = builder.config().chain.clone();
            let handle = builder
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                .extend_rpc_modules(move |ctx| {
                    let api_ext = EthApiExt::new(
                        ctx.registry.eth_api().clone(),
                        Arc::clone(&cache_clone),
                        chain_spec.clone(),
                    );
                    ctx.modules.replace_configured(api_ext.into_rpc())?;
                    Ok(())
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
                    builder.task_executor().spawn(async move {
                        flashblocks_client
                            .init(flashblocks_rollup_args.websocket_url.clone())
                            .unwrap();
                    });
                    builder.task_executor().spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(2));
                        loop {
                            interval.tick().await;
                            cache.cleanup_expired();
                        }
                    });
                    builder.launch_with(launcher)
                })
                .await?;

            handle.wait_for_node_exit().await
        })
        .unwrap();
}
