#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use base_client_node::BaseNodeRunner;
use base_flashblocks_node::{FlashblocksConfig, FlashblocksExtension};
use base_metering::{MeteringConfig, MeteringExtension};
use base_txpool::{TxPoolExtension, TxpoolConfig};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    // Step 1: Initialize versioning so logs / telemetry report the right build info.
    base_cli_utils::Version::init();

    // Step 2: Parse CLI arguments and hand execution to the Optimism node runner.
    use clap::Parser;
    use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
    let cli = Cli::<OpChainSpecParser, cli::Args>::parse();

    // Step 3: Hand the parsed CLI to the node runner so it can build and launch the Base node.
    cli.run(|builder, args| async move {
        let mut runner = BaseNodeRunner::new(args.rollup_args.clone());

        // Create flashblocks config first so we can share its state with metering
        let flashblocks_config: Option<FlashblocksConfig> = args.clone().into();

        // Feature extensions (FlashblocksExtension must be last - uses replace_configured)
        runner.install_ext::<TxPoolExtension>(TxpoolConfig {
            tracing_enabled: args.enable_transaction_tracing,
            tracing_logs_enabled: args.enable_transaction_tracing_logs,
            sequencer_rpc: args.rollup_args.sequencer,
            flashblocks_config: flashblocks_config.clone(),
        });
        runner.install_ext::<MeteringExtension>(MeteringConfig {
            enabled: args.enable_metering,
            flashblocks_config: flashblocks_config.clone(),
        });
        runner.install_ext::<FlashblocksExtension>(flashblocks_config);

        let handle = runner.run(builder);
        handle.await
    })
    .unwrap();
}
