#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use base_builder_core::{
    BaseApiExtServer, BuilderConfig, FlashblocksServiceBuilder, OpEngineApiBuilder, TxDataStoreExt,
};
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::{
    OpNode,
    node::{OpAddOns, OpAddOnsBuilder, OpEngineValidatorBuilder, OpPoolBuilder},
};
use reth_optimism_rpc::OpEthApiBuilder;
use reth_optimism_txpool::OpPooledTransaction;

type BuilderCli = Cli<OpChainSpecParser, cli::Args>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::init_common!();
    base_cli_utils::init_reth!();

    let cli = base_cli_utils::parse_cli!(BuilderCli);

    cli.run(|builder, builder_args| async move {
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
    })
    .unwrap();
}
