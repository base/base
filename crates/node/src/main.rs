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
use reth::providers::StateProviderFactory;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::args::RollupArgs;
use reth_optimism_node::OpNode;
use reth_rpc_eth_api::RpcNodeCore;
use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
struct FlashblocksRollupArgs {
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    #[arg(
        long = "receipt-buffer-size",
        value_name = "RECEIPT_BUFFER_SIZE",
        default_value = "2000",
        env = "RECEIPT_BUFFER_SIZE"
    )]
    pub receipt_buffer_size: usize,

    #[arg(
        long = "total-timeout-secs",
        value_name = "TOTAL_TIMEOUT_SECS",
        default_value = "4",
        env = "TOTAL_TIMEOUT_SECS"
    )]
    pub total_timeout_secs: u64,
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

            let cache = Arc::new(Cache::default());
            let cache_clone = cache.clone();
            let op_node = OpNode::new(flashblocks_rollup_args.rollup_args.clone());
            let receipt_buffer_size = flashblocks_rollup_args.receipt_buffer_size;
            let total_timeout_secs = flashblocks_rollup_args.total_timeout_secs;
            let chain_spec = builder.config().chain.clone();
            let flashblocks_enabled = flashblocks_rollup_args.flashblocks_enabled();

            let handle = builder
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                .extend_rpc_modules(move |ctx| {
                    if flashblocks_enabled {
                        info!(message = "starting flashblocks integration");
                        let mut flashblocks_client =
                            FlashblocksClient::new(cache.clone(), receipt_buffer_size, ctx.provider().clone());

                        flashblocks_client
                            .init(flashblocks_rollup_args.websocket_url.unwrap().clone())
                            .unwrap();
                        let api_ext = EthApiExt::new(
                            ctx.registry.eth_api().clone(),
                            cache.clone(),
                            chain_spec.clone(),
                            flashblocks_client,
                            total_timeout_secs,
                        );
                        ctx.modules.replace_configured(api_ext.into_rpc())?;
                    } else {
                        info!(message = "flashblocks integration is disabled");
                    }
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

                    if flashblocks_enabled {
                        builder.task_executor().spawn(async move {
                            let mut interval = tokio::time::interval(Duration::from_secs(2));
                            loop {
                                interval.tick().await;
                                cache_clone.cleanup_expired();
                            }
                        });
                    }
                    builder.launch_with(launcher)
                })
                .await?;

            handle.wait_for_node_exit().await
        })
        .unwrap();
}
