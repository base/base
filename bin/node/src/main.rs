#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use base_execution_cli::{Cli, chainspec::OpChainSpecParser};
use base_flashblocks::FlashblocksConfig;
use base_flashblocks_node::FlashblocksExtension;
use base_metering::{MeteringConfig, MeteringExtension};
use base_node_runner::BaseNodeRunner;
use base_proofs_extension::ProofsHistoryExtension;
use base_txpool_rpc::{TxPoolRpcConfig, TxPoolRpcExtension};
use base_txpool_tracing::{TxPoolExtension, TxpoolConfig};

type NodeCli = Cli<OpChainSpecParser, cli::Args>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::init_common!();
    base_cli_utils::init_reth!();

    let cli = base_cli_utils::parse_cli!(NodeCli);

    cli.run(|builder, args| async move {
        let mut runner = BaseNodeRunner::new(args.rollup_args.clone());

        // Create flashblocks config first so we can share its state with metering
        let flashblocks_config: Option<FlashblocksConfig> = (&args).into();

        // Feature extensions (FlashblocksExtension must be last - uses replace_configured)
        runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig {
            sequencer_rpc: args.rollup_args.sequencer.clone(),
        });
        runner.install_ext::<TxPoolExtension>(TxpoolConfig {
            tracing_enabled: args.enable_transaction_tracing,
            tracing_logs_enabled: args.enable_transaction_tracing_logs,
            flashblocks_config: flashblocks_config.clone(),
        });
        let metering_config = if args.enable_metering {
            flashblocks_config
                .clone()
                .map_or_else(MeteringConfig::enabled, MeteringConfig::with_flashblocks)
        } else {
            MeteringConfig::disabled()
        };
        runner.install_ext::<MeteringExtension>(metering_config);
        runner.install_ext::<FlashblocksExtension>(flashblocks_config);
        runner.install_ext::<ProofsHistoryExtension>(args.rollup_args);

        runner.run(builder).await
    })
    .unwrap();
}
