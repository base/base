#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;

use base_reth_runner::{
    BaseNodeRunner, BaseRpcExtension, EncryptedRelayExtension, FlashblocksCanonExtension,
    TransactionTracingExtension,
};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    // Step 1: Initialize versioning so logs / telemetry report the right build info.
    base_reth_cli::Version::init();

    // Step 2: Parse CLI arguments and hand execution to the Optimism node runner.
    use clap::Parser;
    use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
    let cli = Cli::<OpChainSpecParser, cli::Args>::parse();

    // Step 3: Hand the parsed CLI to the node runner so it can build and launch the Base node.
    cli.run(|builder, args| async move {
        let mut runner = BaseNodeRunner::new(args);
        runner.install_ext::<FlashblocksCanonExtension>()?;
        runner.install_ext::<TransactionTracingExtension>()?;
        runner.install_ext::<BaseRpcExtension>()?;
        runner.install_ext::<EncryptedRelayExtension>()?;
        let handle = runner.run(builder);
        handle.await
    })
    .unwrap();
}
