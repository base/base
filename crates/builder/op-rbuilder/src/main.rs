use args::CliExt;
use clap::Parser;
use reth_optimism_cli::{chainspec::OpChainSpecParser, Cli};
use reth_optimism_node::{
    node::{OpAddOnsBuilder, OpPoolBuilder},
    OpNode,
};
use reth_transaction_pool::TransactionPool;

/// CLI argument parsing.
pub mod args;
pub mod generator;
mod metrics;
mod monitor_tx_pool;
#[cfg(feature = "flashblocks")]
pub mod payload_builder;
mod primitives;
mod revert_protection;
mod tx;
mod tx_signer;

#[cfg(not(feature = "flashblocks"))]
mod payload_builder_vanilla;

#[cfg(not(feature = "flashblocks"))]
use payload_builder_vanilla::CustomOpPayloadBuilder;

#[cfg(feature = "flashblocks")]
use payload_builder::CustomOpPayloadBuilder;

use metrics::{
    VersionInfo, BUILD_PROFILE_NAME, CARGO_PKG_VERSION, VERGEN_BUILD_TIMESTAMP,
    VERGEN_CARGO_FEATURES, VERGEN_CARGO_TARGET_TRIPLE, VERGEN_GIT_SHA,
};
use monitor_tx_pool::monitor_tx_pool;
use revert_protection::{EthApiOverrideServer, RevertProtectionExt};
use tx::FBPooledTransaction;

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    let version = VersionInfo {
        version: CARGO_PKG_VERSION,
        build_timestamp: VERGEN_BUILD_TIMESTAMP,
        cargo_features: VERGEN_CARGO_FEATURES,
        git_sha: VERGEN_GIT_SHA,
        target_triple: VERGEN_CARGO_TARGET_TRIPLE,
        build_profile: BUILD_PROFILE_NAME,
    };

    Cli::<OpChainSpecParser, args::OpRbuilderArgs>::parse()
        .populate_defaults()
        .run(|builder, builder_args| async move {
            let rollup_args = builder_args.rollup_args;

            let op_node = OpNode::new(rollup_args.clone());
            let handle = builder
                .with_types::<OpNode>()
                .with_components(
                    op_node
                        .components()
                        .pool(
                            OpPoolBuilder::<FBPooledTransaction>::default()
                                .with_enable_tx_conditional(
                                    // Revert protection uses the same internal pool logic as conditional transactions
                                    // to garbage collect transactions out of the bundle range.
                                    rollup_args.enable_tx_conditional
                                        || builder_args.enable_revert_protection,
                                )
                                .with_supervisor(
                                    rollup_args.supervisor_http.clone(),
                                    rollup_args.supervisor_safety_level,
                                ),
                        )
                        .payload(CustomOpPayloadBuilder::new(
                            builder_args.builder_signer,
                            std::time::Duration::from_secs(builder_args.extra_block_deadline_secs),
                            builder_args.flashblocks_ws_url,
                            builder_args.chain_block_time,
                            builder_args.flashblock_block_time,
                        )),
                )
                .with_add_ons(
                    OpAddOnsBuilder::default()
                        .with_sequencer(rollup_args.sequencer.clone())
                        .with_enable_tx_conditional(rollup_args.enable_tx_conditional)
                        .build(),
                )
                .extend_rpc_modules(move |ctx| {
                    if builder_args.enable_revert_protection {
                        tracing::info!("Revert protection enabled");

                        let pool = ctx.pool().clone();
                        let provider = ctx.provider().clone();
                        let revert_protection_ext = RevertProtectionExt::new(pool, provider);

                        ctx.modules
                            .merge_configured(revert_protection_ext.into_rpc())?;
                    }

                    Ok(())
                })
                .on_node_started(move |ctx| {
                    version.register_version_metrics();
                    if builder_args.log_pool_transactions {
                        tracing::info!("Logging pool transactions");
                        ctx.task_executor.spawn_critical(
                            "txlogging",
                            Box::pin(async move {
                                monitor_tx_pool(ctx.pool.all_transactions_event_listener()).await;
                            }),
                        );
                    }

                    Ok(())
                })
                .launch()
                .await?;

            handle.node_exit_future.await
        })
        .unwrap();
}
