use clap::Parser;
use generator::CustomOpPayloadBuilder;
use monitoring::Monitoring;
use payload_builder_vanilla::OpPayloadBuilderVanilla;
use reth::builder::Node;
use reth::providers::CanonStateSubscriptions;
use reth::{
    builder::{engine_tree_config::TreeConfig, EngineNodeLauncher},
    providers::providers::BlockchainProvider2,
};
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::OpNode;

/// CLI argument parsing.
pub mod args;

pub mod generator;
#[cfg(test)]
mod integration;
mod metrics;
mod monitoring;
pub mod payload_builder;
mod payload_builder_vanilla;
#[cfg(test)]
mod tester;
mod tx_signer;

fn main() {
    Cli::<OpChainSpecParser, args::OpRbuilderArgs>::parse()
        .run(|builder, builder_args| async move {
            let rollup_args = builder_args.rollup_args;

            let vanilla_builder = OpPayloadBuilderVanilla::new(
                OpEvmConfig::new(builder.config().chain.clone()),
                builder_args.builder_signer,
            );

            let engine_tree_config = TreeConfig::default()
                .with_persistence_threshold(rollup_args.persistence_threshold)
                .with_memory_block_buffer_target(rollup_args.memory_block_buffer_target);

            let op_node = OpNode::new(rollup_args.clone());
            let handle = builder
                .with_types_and_provider::<OpNode, BlockchainProvider2<_>>()
                .with_components(
                    op_node
                        .components()
                        .payload(CustomOpPayloadBuilder::new(vanilla_builder)),
                )
                .with_add_ons(op_node.add_ons())
                /*
                // TODO: ExEx in Op-reth fails from time to time when restarting the node.
                // Switching to the spawn task on the meantime.
                // https://github.com/paradigmxyz/reth/issues/14360
                .install_exex("monitoring", move |ctx| {
                    let builder_signer = builder_args.builder_signer;
                    if let Some(signer) = &builder_signer {
                        tracing::info!("Builder signer address is set to: {:?}", signer.address);
                    } else {
                        tracing::info!("Builder signer is not set");
                    }
                    async move { Ok(Monitoring::new(builder_signer).run_with_exex(ctx)) }
                })
                */
                .on_node_started(move |ctx| {
                    let new_canonical_blocks = ctx.provider().canonical_state_stream();
                    let builder_signer = builder_args.builder_signer;

                    ctx.task_executor.spawn_critical(
                        "monitoring",
                        Box::pin(async move {
                            let monitoring = Monitoring::new(builder_signer);
                            let _ = monitoring.run_with_stream(new_canonical_blocks).await;
                        }),
                    );

                    Ok(())
                })
                .launch_with_fn(|builder| {
                    let launcher = EngineNodeLauncher::new(
                        builder.task_executor().clone(),
                        builder.config().datadir(),
                        engine_tree_config,
                    );
                    builder.launch_with(launcher)
                })
                .await?;

            handle.node_exit_future.await
        })
        .unwrap();
}
