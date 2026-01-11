#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use std::sync::Arc;

use base_client_primitives::{BaseNodeRunner, OpProvider};
use base_flashblocks::{FlashblocksCell, FlashblocksExtension, FlashblocksState};
use base_metering::MeteringExtension;
use base_txpool::TxPoolExtension;
use once_cell::sync::OnceCell;

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
        let flashblocks_cell: FlashblocksCell<FlashblocksState<OpProvider>> =
            Arc::new(OnceCell::new());

        let sequencer_rpc = args.rollup_args.sequencer.clone();
        let tracing_config = args.tracing_config();
        let metering_enabled = args.enable_metering;
        let flashblocks_config = args.flashblocks_config();

        let mut runner = BaseNodeRunner::new(args.rollup_args);

        // Feature extensions (FlashblocksExtension must be last - uses replace_configured)
        runner.install_ext(Box::new(TxPoolExtension::new(tracing_config, sequencer_rpc)));
        runner.install_ext(Box::new(MeteringExtension::new(metering_enabled)));
        runner
            .install_ext(Box::new(FlashblocksExtension::new(flashblocks_cell, flashblocks_config)));

        let handle = runner.run(builder);
        handle.await
    })
    .unwrap();
}
