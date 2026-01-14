use core::fmt::Debug;
use std::{marker::PhantomData, sync::Arc};

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
    args::OpRbuilderArgs,
    builders::{BuilderConfig, PayloadBuilder},
    metrics::{VERSION, record_flag_gauge_metrics},
    primitives::reth::engine_api_builder::OpEngineApiBuilder,
    tx_data_store::{BaseApiExtServer, TxDataStoreExt},
};

/// Launcher for the OP builder node.
#[derive(Debug)]
pub struct BuilderLauncher<B> {
    _builder: PhantomData<B>,
}

impl<B> BuilderLauncher<B>
where
    B: PayloadBuilder,
{
    pub const fn new() -> Self {
        Self { _builder: PhantomData }
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

        record_flag_gauge_metrics(&builder_args);

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();
        let rollup_args = builder_args.rollup_args;
        let op_node = OpNode::new(rollup_args.clone());
        let tx_data_store = builder_config.tx_data_store.clone();

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
                    .payload(B::new_service(builder_config)?),
            )
            .with_add_ons(addons)
            .extend_rpc_modules(move |ctx| {
                let tx_data_store_ext = TxDataStoreExt::new(tx_data_store);
                ctx.modules.add_or_replace_configured(tx_data_store_ext.into_rpc())?;

                Ok(())
            })
            .on_node_started(move |_ctx| {
                VERSION.register_version_metrics();
                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await?;
        Ok(())
    }
}
