//! The main entry point for `op-rbuilder`.
//!
//! `op-rbuilder` is an OP Stack EL client with block building capabilities, powered by in-process
//! rbuilder.
//!
//! The primary difference between `op-rbuilder` and `op-reth` is the `PayloadBuilder` derives
//! transactions exclusively from rbuilder, rather than directly from its transaction pool.
//!
//! ## Usage
//!
//! It has a new mandatory cli arg `--rbuilder.config` which must point to an rbuilder config file.
//!
//! ## Demo
//!
//! Instructions to demo `op-rbuilder` building blocks for an OP L2, and send txns to it with `mev-flood`:
//!
//! 1. Clone [flashbots/optimism](https://github.com/flashbots/optimism) and checkout the
//!    `op-rbuilder` branch.
//! 2. `rm` any existing `reth` chain db
//! 3. Run a clean OP stack: `make devnet-clean && make devnet-down && make devnet-up`
//! 4. Run `op-rbuilder` on port 8547: `cargo run --bin op-rbuilder --features "optimism,jemalloc" -- node
//!    --chain ../optimism/.devnet/genesis-l2.json --http --http.port 8547 --authrpc.jwtsecret
//!    ../optimism/ops-bedrock/test-jwt-secret.txt --rbuilder.config config-optimism-local.toml`
//! 5. Init `mev-flood`: `docker run mevflood init -r http://host.docker.internal:8547 -s local.json`
//! 6. Run `mev-flood`: `docker run --init -v ${PWD}:/app/cli/deployments mevflood spam -p 3 -t 5 -r http://host.docker.internal:8547 -l local.json`
//!
//! Example starting clean OP Stack in one-line: `rm -rf /Users/liamaharon/Library/Application\ Support/reth && cd ../optimism && make devnet-clean && make devnet-down && make devnet-up && cd ../rbuilder && cargo run --bin op-rbuilder --features "optimism,jemalloc" -- node --chain ../optimism/.devnet/genesis-l2.json --http --http.port 8547 --authrpc.jwtsecret ../optimism/ops-bedrock/test-jwt-secret.txt --rbuilder.config config-optimism-local.toml`

#![cfg_attr(all(not(test), feature = "optimism"), warn(unused_crate_dependencies))]
// The `optimism` feature must be enabled to use this crate.
#![cfg(feature = "optimism")]

mod eth_bundle_api;

use crate::eth_bundle_api::EthCallBundleMinimalApiServer;
use clap_builder::Parser;
use eth_bundle_api::EthBundleMinimalApi;
use op_rbuilder_node_optimism::{args::OpRbuilderArgs, OpRbuilderNode};
use reth::cli::Cli;
use reth_node_optimism::node::OptimismAddOns;
use reth_optimism_rpc::eth::rpc::SequencerClient;
use tracing as _;

// jemalloc provides better performance
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) = Cli::<OpRbuilderArgs>::parse().run(|builder, op_rbuilder_args| async move {
        let sequencer_http_arg = op_rbuilder_args.sequencer_http.clone();
        let handle = builder
            .with_types::<OpRbuilderNode>()
            .with_components(OpRbuilderNode::components(op_rbuilder_args))
            .with_add_ons::<OptimismAddOns>()
            .extend_rpc_modules(move |ctx| {
                // register sequencer tx forwarder
                if let Some(sequencer_http) = sequencer_http_arg {
                    ctx.registry
                        .eth_api()
                        .set_sequencer_client(SequencerClient::new(sequencer_http));
                }

                // register eth bundle api
                let ext = EthBundleMinimalApi::new(ctx.registry.pool().clone());
                ctx.modules.merge_configured(ext.into_rpc())?;

                Ok(())
            })
            .launch()
            .await?;

        handle.node_exit_future.await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
