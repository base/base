use base_reth_flashblocks_rpc::{cache::Cache, flashblocks::FlashblocksClient, rpc::EthApiExt};
use std::sync::Arc;
use std::time::Duration;

use base_reth_flashblocks_rpc::rpc::EthApiOverrideServer;
use base_reth_mempool_tracer::args::MempoolTracerArgs;
use clap::Parser;
use reth::builder::{FullNode, Node, NodeHandle};
use reth::{
    providers::providers::BlockchainProvider,
};
use reth::api::{FullNodeComponents, NodeAddOns};
use reth::transaction_pool::{FullTransactionEvent, TransactionPool};
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::args::RollupArgs;
use reth_optimism_node::OpNode;
use tokio_stream::StreamExt;
use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
struct FlashblockRollupArgs {
    #[arg(long = "rollup.flashblocks.url", value_name = "ROLLUP_FLASHBLOCKS_URL")]
    pub flashblocks_url: String,

    #[arg(long = "rollup.flashblocks.enabled", value_name = "ROLLUP_FLASHBLOCKS_ENABLED")]
    pub flashblocks_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
struct BaseRollupArgs {
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    #[command(flatten)]
    pub flashblock_args: FlashblockRollupArgs,

    #[command(flatten)]
    pub mempool_tracer_args: MempoolTracerArgs,
}

fn start_mempool_tracer<Node: FullNodeComponents, AddOns: NodeAddOns<Node>>(node: &FullNode<Node, AddOns>) {
    println!("setup mempool tracing");
    // let mempool_tracer = LoggingMempoolTracer::default();
    let mut txns = node.pool.all_transactions_event_listener();

    node.task_executor.spawn(async move {
        while let Some(event) = txns.next().await {
            println!("Transaction received: {event:?}");
            match event {
                FullTransactionEvent::Pending(_) => {}
                FullTransactionEvent::Queued(_) => {}
                FullTransactionEvent::Mined { .. } => {}
                FullTransactionEvent::Replaced { .. } => {}
                FullTransactionEvent::Discarded(_) => {}
                FullTransactionEvent::Invalid(_) => {}
                FullTransactionEvent::Propagated(_) => {}
            }
        }
    });
}

fn start_flashblocks<Node: FullNodeComponents, AddOns: NodeAddOns<Node>>(
    node: &FullNode<Node, AddOns>,
    args: FlashblockRollupArgs,
    cache: Arc<Cache>,
) {
    let mut flashblocks_client = FlashblocksClient::new(cache.clone());

    node.task_executor.spawn(async move {
        flashblocks_client
            .init(args.flashblocks_url.clone())
            .unwrap();
    });

    node.task_executor.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            cache.cleanup_expired();
        }
    });
}

fn main() {
    Cli::<OpChainSpecParser, BaseRollupArgs>::parse()
        .run(|builder, args| async move {
            info!("Starting Base Reth node");
            let op_node = OpNode::new(args.rollup_args.clone());

            let chain_spec = builder.config().chain.clone();
            let cache = Arc::new(Cache::default());
            let cache_clone = cache.clone();

            let NodeHandle { node, node_exit_future } = builder
                .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| Ok(()))
                .extend_rpc_modules(move |ctx| {
                    let api_ext = EthApiExt::new(
                        ctx.registry.eth_api().clone(),
                        cache_clone,
                        chain_spec.clone(),
                    );
                    ctx.modules.replace_configured(api_ext.into_rpc())?;
                    Ok(())
                })
                .launch().await?;

            start_flashblocks(&node, args.flashblock_args, cache);
            start_mempool_tracer(&node);

            node_exit_future.await
        })
        .unwrap();
}

