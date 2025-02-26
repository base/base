use clap::Parser;
use monitoring::Monitoring;
use payload_builder_vanilla::CustomOpPayloadBuilder;
use reth::providers::CanonStateSubscriptions;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::node::OpAddOnsBuilder;
use reth_optimism_node::OpNode;

/// CLI argument parsing.
pub mod args;

pub mod generator;
#[cfg(test)]
mod integration;
mod metrics;
mod monitoring;
// pub mod payload_builder;
mod payload_builder_vanilla;
#[cfg(test)]
mod tester;
mod tx_signer;

fn main() {
    Cli::<OpChainSpecParser, args::OpRbuilderArgs>::parse()
        .run(|builder, builder_args| async move {
            let rollup_args = builder_args.rollup_args;

            let op_node = OpNode::new(rollup_args.clone());
            let handle = builder
                .with_types::<OpNode>()
                .with_components(
                    op_node
                        .components()
                        .payload(CustomOpPayloadBuilder::new(builder_args.builder_signer)),
                )
                .with_add_ons(
                    OpAddOnsBuilder::default()
                        .with_sequencer(rollup_args.sequencer_http.clone())
                        .with_enable_tx_conditional(rollup_args.enable_tx_conditional)
                        .build(),
                )
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
                .launch()
                .await?;

            handle.node_exit_future.await
        })
        .unwrap();
}
