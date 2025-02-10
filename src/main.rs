mod cache;
mod rpc;
mod subscriber;

use crate::cache::Cache;
use crate::rpc::{BaseApiExt, BaseApiServer, EthApiExt, EthApiOverrideServer};
use clap::Parser;
use reth::builder::Node;
use reth::{
    builder::{engine_tree_config::TreeConfig, EngineNodeLauncher},
    providers::providers::BlockchainProvider2,
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

    #[arg(
        long = "redis-url",
        value_name = "REDIS_URL",
        default_value = "redis://localhost:6379"
    )]
    pub redis_url: String,

    #[arg(long = "producer-url", value_name = "PRODUCER_URL")]
    pub producer_url: String,
}

fn main() {
    Cli::<OpChainSpecParser, FlashblocksRollupArgs>::parse()
        .run(|builder, flashblocks_rollup_args| async move {
            info!("Starting custom Base node");

            let cache = Cache::new(flashblocks_rollup_args.redis_url.as_str()).unwrap();
            let subscriber = subscriber::Subscriber::new(
                cache.clone(),
                flashblocks_rollup_args.producer_url.clone(),
            );

            let op_node = OpNode::new(flashblocks_rollup_args.rollup_args.clone());

            let handle = builder
                .with_types_and_provider::<OpNode, BlockchainProvider2<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                .extend_rpc_modules(move |ctx| {
                    let api_ext = EthApiExt::new(ctx.registry.eth_api().clone(), cache);
                    ctx.modules.replace_configured(api_ext.into_rpc())?;

                    let base_ext = BaseApiExt {};
                    ctx.modules.merge_http(base_ext.into_rpc())?;

                    Ok(())
                })
                .launch_with_fn(|builder| {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(
                            flashblocks_rollup_args.rollup_args.persistence_threshold,
                        )
                        .with_memory_block_buffer_target(
                            flashblocks_rollup_args
                                .rollup_args
                                .memory_block_buffer_target,
                        );
                    let launcher = EngineNodeLauncher::new(
                        builder.task_executor().clone(),
                        builder.config().datadir(),
                        engine_tree_config,
                    );
                    builder.task_executor().spawn(async move {
                        if let Err(e) = subscriber.subscribe_to_sse().await {
                            eprintln!("Error subscribing to SSE: {}", e);
                        }
                    });
                    builder.launch_with(launcher)
                })
                .await?;

            handle.wait_for_node_exit().await
        })
        .unwrap();
}
