#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use base_builder_core::{BuilderConfig, FlashblocksServiceBuilder};
use base_builder_metering::MeteringStoreExtension;
use base_client_node::BaseNodeRunner;
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};
use base_validated_tx_rpc::{ValidatedTxRpcConfig, ValidatedTxRpcExtension};
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};

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
        let metering_store = builder_args.build_metering_store();

        let mut runner = BaseNodeRunner::new(builder_args.rollup_args.clone())
            .with_service_builder(FlashblocksServiceBuilder(builder_config));
        runner.install_ext::<MeteringStoreExtension>(metering_store);
        runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig::default());
        runner.install_ext::<ValidatedTxRpcExtension>(ValidatedTxRpcConfig::default());

        runner.run(builder).await
    })
    .unwrap();
}
