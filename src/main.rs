use clap::Parser;
use reth::builder::Node;
use reth::{
    builder::{engine_tree_config::TreeConfig, EngineNodeLauncher},
    providers::providers::BlockchainProvider2,
};
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::args::RollupArgs;
use reth_optimism_node::OpNode;

fn main() {
    Cli::<OpChainSpecParser, RollupArgs>::parse()
        .run(|builder, rollup_args| async move {
            let op_node = OpNode::new(rollup_args.clone());

            let handle = builder
                .with_types_and_provider::<OpNode, BlockchainProvider2<_>>()
                .with_components(op_node.components())
                .with_add_ons(op_node.add_ons())
                .on_component_initialized(move |_ctx| {
                    Ok(())
                })
                .extend_rpc_modules(move |_ctx| {
                    Ok(())
                })
                .launch_with_fn(|builder| {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(rollup_args.persistence_threshold)
                        .with_memory_block_buffer_target(rollup_args.memory_block_buffer_target);
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
