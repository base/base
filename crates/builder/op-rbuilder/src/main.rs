use args::*;
use builders::{BuilderMode, FlashblocksBuilder, StandardBuilder};
use eyre::Result;

/// CLI argument parsing.
pub mod args;
mod builders;
mod metrics;
mod monitor_tx_pool;
mod primitives;
mod revert_protection;
mod traits;
mod tx;
mod tx_signer;

use builders::{BuilderConfig, PayloadBuilder};
use core::fmt::Debug;
use metrics::VERSION;
use moka::future::Cache;
use monitor_tx_pool::monitor_tx_pool;
use reth::builder::{NodeBuilder, WithLaunchContext};
use reth_cli_commands::launcher::Launcher;
use reth_db::mdbx::DatabaseEnv;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::chainspec::OpChainSpecParser;
use reth_optimism_node::{
    node::{OpAddOnsBuilder, OpPoolBuilder},
    OpNode,
};
use reth_transaction_pool::TransactionPool;
use revert_protection::{EthApiExtServer, EthApiOverrideServer, RevertProtectionExt};
use std::{marker::PhantomData, sync::Arc};
use tx::FBPooledTransaction;

// Prefer jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> Result<()> {
    let cli = Cli::parsed();
    let mode = cli.builder_mode();
    let mut cli_app = cli.configure();

    #[cfg(feature = "telemetry")]
    {
        let otlp = reth_tracing_otlp::layer("op-reth");
        cli_app.access_tracing_layers()?.add_layer(otlp);
    }

    cli_app.init_tracing()?;
    match mode {
        BuilderMode::Standard => {
            tracing::info!("Starting OP builder in standard mode");
            let launcher = BuilderLauncher::<StandardBuilder>::new();
            cli_app.run(launcher)?;
        }
        BuilderMode::Flashblocks => {
            tracing::info!("Starting OP builder in flashblocks mode");
            let launcher = BuilderLauncher::<FlashblocksBuilder>::new();
            cli_app.run(launcher)?;
        }
    }
    Ok(())
}

pub struct BuilderLauncher<B> {
    _builder: PhantomData<B>,
}

impl<B> BuilderLauncher<B>
where
    B: PayloadBuilder,
{
    pub fn new() -> Self {
        Self {
            _builder: PhantomData,
        }
    }
}

impl<B> Default for BuilderLauncher<B>
where
    B: PayloadBuilder,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<B> Launcher<OpChainSpecParser, OpRbuilderArgs> for BuilderLauncher<B>
where
    B: PayloadBuilder,
    BuilderConfig<B::Config>: TryFrom<OpRbuilderArgs>,
    <BuilderConfig<B::Config> as TryFrom<OpRbuilderArgs>>::Error: Debug,
{
    async fn entrypoint(
        self,
        builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>,
        builder_args: OpRbuilderArgs,
    ) -> Result<()> {
        let builder_config = BuilderConfig::<B::Config>::try_from(builder_args.clone())
            .expect("Failed to convert rollup args to builder config");
        let da_config = builder_config.da_config.clone();
        let rollup_args = builder_args.rollup_args;
        let op_node = OpNode::new(rollup_args.clone());
        let reverted_cache = Cache::builder().max_capacity(100).build();
        let reverted_cache_copy = reverted_cache.clone();

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
                    .payload(B::new_service(builder_config)?),
            )
            .with_add_ons(
                OpAddOnsBuilder::default()
                    .with_sequencer(rollup_args.sequencer.clone())
                    .with_enable_tx_conditional(rollup_args.enable_tx_conditional)
                    .with_da_config(da_config)
                    .build(),
            )
            .extend_rpc_modules(move |ctx| {
                if builder_args.enable_revert_protection {
                    tracing::info!("Revert protection enabled");

                    let pool = ctx.pool().clone();
                    let provider = ctx.provider().clone();
                    let revert_protection_ext: RevertProtectionExt<
                        _,
                        _,
                        _,
                        op_alloy_network::Optimism,
                    > = RevertProtectionExt::new(pool, provider, ctx.registry.eth_api().clone());

                    ctx.modules
                        .merge_configured(revert_protection_ext.bundle_api().into_rpc())?;
                    ctx.modules.replace_configured(
                        revert_protection_ext.eth_api(reverted_cache).into_rpc(),
                    )?;
                }

                Ok(())
            })
            .on_node_started(move |ctx| {
                VERSION.register_version_metrics();
                if builder_args.log_pool_transactions {
                    tracing::info!("Logging pool transactions");
                    ctx.task_executor.spawn_critical(
                        "txlogging",
                        Box::pin(async move {
                            monitor_tx_pool(
                                ctx.pool.all_transactions_event_listener(),
                                reverted_cache_copy,
                            )
                            .await;
                        }),
                    );
                }

                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await?;
        Ok(())
    }
}
