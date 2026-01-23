#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[macro_use]
extern crate tracing;

pub mod cli;
pub mod consensus;
pub mod engine;
pub mod execution;
pub mod setup;
pub mod version;

use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::Backtracing::enable();
    base_cli_utils::SigsegvHandler::install();

    Cli::<OpChainSpecParser, cli::UnifiedArgs>::parse()
        .run(|builder, args| async move {
            // Setup tracing and metrics.
            setup::Setup::init(&args)?;

            // Grab the engine endpoint and chain ID from the builder config.
            let chain_id = builder.config().chain.chain().id();
            let engine_endpoint = engine::EngineEndpoint::from_reth_config(builder.config())?;

            // Start reth and get its handle.
            let execution = execution::Execution::new(args.rollup_args);
            let exec_handle = execution.run(builder);

            // Start consensus and get its handle.
            let consensus = consensus::Consensus::new(engine_endpoint, chain_id);
            let consensus_handle = consensus.run();

            // Wait for either to exit.
            tokio::select! {
                res = exec_handle => {
                    info!(target: "unified", "Execution client exited");
                    res
                }
                res = consensus_handle => {
                    info!(target: "unified", "Consensus client exited");
                    res
                }
            }
        })
        .unwrap();
}
