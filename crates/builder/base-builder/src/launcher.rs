use std::sync::Arc;

use base_builder_cli::{Cli, OpRbuilderArgs};
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
    flashblocks::{BuilderConfig, FlashblocksServiceBuilder},
    BaseApiExtServer, OpEngineApiBuilder, TxDataStoreExt,
};

pub fn launch(cli: Cli) -> Result<()> {
    let telemetry_args = match &cli.command {
        reth_optimism_cli::commands::Commands::Node(node_command) => {
            node_command.ext.telemetry.clone()
        }
        _ => Default::default(),
    };

    let mut cli_app = cli.configure();

    // Only setup telemetry if an OTLP endpoint is provided
    if telemetry_args.otlp_endpoint.is_some() {
        let telemetry_layer = telemetry_args.setup()?;
        cli_app.access_tracing_layers()?.add_layer(telemetry_layer);
    }

    tracing::info!("Starting OP builder in flashblocks mode");
    let launcher = BuilderLauncher::new();
    cli_app.run(launcher)?;
    Ok(())
}

#[derive(Debug)]
pub struct BuilderLauncher;

impl BuilderLauncher {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for BuilderLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl Launcher<OpChainSpecParser, OpRbuilderArgs> for BuilderLauncher {
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

        let addons: OpAddOns<
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
                base_cli_utils::register_version_metrics!();
                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await?;
        Ok(())
    }
}
