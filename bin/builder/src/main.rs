#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use std::sync::Arc;

use base_builder_core::{BuilderApiExtension, FlashblocksServiceBuilder};
use base_builder_metering::MeteringStoreExtension;
use base_execution_cli::{Cli, chainspec::OpChainSpecParser};
use base_node_runner::BaseNodeRunner;
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};

type BuilderCli = Cli<OpChainSpecParser, cli::Args>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::init_common!();
    base_cli_utils::init_reth!();

    let cli = base_cli_utils::parse_cli!(BuilderCli);

    cli.run(|builder, builder_args| async move {
        let metering_provider: base_builder_core::SharedMeteringProvider =
            Arc::new(builder_args.build_metering_store());
        let rollup_args = builder_args.rollup_args.clone();
        let builder_config = builder_args
            .into_builder_config(Arc::clone(&metering_provider))
            .expect("Failed to convert rollup args to builder config");

        let mut runner = BaseNodeRunner::new(rollup_args)
            .with_service_builder(FlashblocksServiceBuilder(builder_config));
        runner.install_ext::<MeteringStoreExtension>(metering_provider);
        runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig::default());
        runner.install_ext::<BuilderApiExtension>(());

        runner.run(builder).await
    })
    .unwrap();
}
