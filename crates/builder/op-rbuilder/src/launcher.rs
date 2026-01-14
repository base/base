use std::sync::Arc;

use eyre::Result;
use reth_cli_commands::launcher::Launcher;
use reth_db::mdbx::DatabaseEnv;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_cli::chainspec::OpChainSpecParser;
use reth_optimism_node::{
    OpNode,
    node::{OpAddOns, OpAddOnsBuilder, OpEngineValidatorBuilder, OpPoolBuilder},
};
use reth_optimism_rpc::OpEthApiBuilder;
use reth_optimism_txpool::OpPooledTransaction;

use crate::{
    args::{Cli, CliExt, OpRbuilderArgs},
    flashblocks::{BuilderConfig, FlashblocksServiceBuilder},
    primitives::reth::engine_api_builder::OpEngineApiBuilder,
    tx_data_store::{BaseApiExtServer, TxDataStoreExt},
};

/// Launches the op-rbuilder node.
///
/// The `on_node_started` callback is invoked after the node has been started successfully.
/// This is typically used to register version metrics.
pub fn launch<F>(on_node_started: F) -> Result<()>
where
    F: Fn() + Send + Sync + 'static,
{
    let cli = Cli::parsed();

    #[cfg(feature = "telemetry")]
    let telemetry_args = match &cli.command {
        reth_optimism_cli::commands::Commands::Node(node_command) => {
            node_command.ext.telemetry.clone()
        }
        _ => Default::default(),
    };

    #[cfg(not(feature = "telemetry"))]
    let cli_app = cli.configure();

    #[cfg(feature = "telemetry")]
    let mut cli_app = cli.configure();
    #[cfg(feature = "telemetry")]
    {
        use crate::primitives::telemetry::setup_telemetry_layer;
        let telemetry_layer = setup_telemetry_layer(&telemetry_args)?;
        cli_app.access_tracing_layers()?.add_layer(telemetry_layer);
    }

    tracing::info!("Starting OP builder in flashblocks mode");
    let launcher = BuilderLauncher::new(on_node_started);
    cli_app.run(launcher)?;
    Ok(())
}

/// The builder launcher that implements the reth [`Launcher`] trait.
pub struct BuilderLauncher<F> {
    on_node_started: F,
}

impl<F> std::fmt::Debug for BuilderLauncher<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuilderLauncher").finish_non_exhaustive()
    }
}

impl<F> BuilderLauncher<F> {
    /// Creates a new [`BuilderLauncher`] with the given callback.
    pub const fn new(on_node_started: F) -> Self {
        Self { on_node_started }
    }
}

impl<F> Launcher<OpChainSpecParser, OpRbuilderArgs> for BuilderLauncher<F>
where
    F: Fn() + Send + Sync + 'static,
{
    async fn entrypoint(
        self,
        builder: WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>,
        builder_args: OpRbuilderArgs,
    ) -> Result<()> {
        let builder_config = BuilderConfig::try_from(builder_args.clone())
            .expect("Failed to convert rollup args to builder config");

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();
        let rollup_args = builder_args.rollup_args;
        let op_node = OpNode::new(rollup_args.clone());
        let tx_data_store = builder_config.tx_data_store.clone();
        let on_node_started = self.on_node_started;

        let mut addons: OpAddOns<
            _,
            OpEthApiBuilder,
            OpEngineValidatorBuilder,
            OpEngineApiBuilder<OpEngineValidatorBuilder>,
        > = OpAddOnsBuilder::default()
            .with_sequencer(rollup_args.sequencer.clone())
            .with_enable_tx_conditional(false)
            .with_da_config(da_config)
            .with_gas_limit_config(gas_limit_config)
            .build();
        if cfg!(feature = "custom-engine-api") {
            let engine_builder: OpEngineApiBuilder<OpEngineValidatorBuilder> =
                OpEngineApiBuilder::default();
            addons = addons.with_engine_api(engine_builder);
        }
        let handle = builder
            .with_types::<OpNode>()
            .with_components(
                op_node
                    .components()
                    .pool(
                        OpPoolBuilder::<OpPooledTransaction>::default()
                            .with_enable_tx_conditional(false)
                            .with_supervisor(
                                rollup_args.supervisor_http.clone(),
                                rollup_args.supervisor_safety_level,
                            ),
                    )
                    .payload(FlashblocksServiceBuilder(builder_config)),
            )
            .with_add_ons(addons)
            .extend_rpc_modules(move |ctx| {
                let tx_data_store_ext = TxDataStoreExt::new(tx_data_store);
                ctx.modules.add_or_replace_configured(tx_data_store_ext.into_rpc())?;

                Ok(())
            })
            .on_node_started(move |_ctx| {
                on_node_started();
                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await?;
        Ok(())
    }
}
