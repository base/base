#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(not(any(target_os = "linux", feature = "local")))]
use base_consensus_registry as _;
#[cfg(not(any(target_os = "linux", feature = "local")))]
use base_proof_tee_nitro_host as _;
use clap::Parser as _;
use serde as _;
use tokio as _;
#[cfg(not(any(target_os = "linux", feature = "local")))]
use tracing as _;

mod cli;

fn main() {
    base_cli_utils::init_common!();

    if let Err(err) = cli::Cli::parse().run() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
