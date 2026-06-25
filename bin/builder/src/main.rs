#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::sync::Arc;

use base_builder_cli::Args;
use base_builder_core::{BuilderApiExtension, FlashblocksServiceBuilder};
use base_builder_metering::MeteringStoreExtension;
use base_execution_cli::{Cli, StandardBaseRethNode};
use base_node_runner::BaseNodeRunner;
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};

type BuilderCli = Cli<Args>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::init_common!();
    base_reth_cli::init_reth!();

    let cli = base_cli_utils::parse_cli!(BuilderCli);

    cli.run(|builder, builder_args| async move {
        let rollup_args = builder_args.rollup_args.clone();
        let builder = StandardBaseRethNode::apply_initial_upgrade_signal_from_rollup_args(
            builder,
            &rollup_args,
        )
        .await?;

        let metering_provider: base_builder_core::SharedMeteringProvider =
            Arc::new(builder_args.build_metering_store());

        let builder_config = builder_args
            .into_builder_config(Arc::clone(&metering_provider))
            .expect("Failed to convert rollup args to builder config");
        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();

        let mut runner = BaseNodeRunner::new(rollup_args.clone())
            .with_da_config(da_config)
            .with_gas_limit_config(gas_limit_config)
            .with_service_builder(FlashblocksServiceBuilder(builder_config));
        runner.install_ext::<MeteringStoreExtension>(metering_provider);
        runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig::default());
        runner.install_ext::<BuilderApiExtension>(());
        StandardBaseRethNode::install_upgrade_signal_metrics_extension(&mut runner, &rollup_args)?;
        runner.add_started_callback(|| {
            base_cli_utils::register_version_metrics!();
            Ok(())
        });

        runner.run(builder).await
    })
    .unwrap();
}
