#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// These crates are only used in cfg-gated code paths (linux / local feature)
// but must be listed as direct deps. Declare them here so the
// `unused_crate_dependencies` lint is satisfied on all platforms.
#[cfg(not(any(target_os = "linux", feature = "local")))]
use base_consensus_registry as _;
#[cfg(not(any(target_os = "linux", feature = "local")))]
use base_proof_host as _;
use clap::Parser as _;
#[cfg(not(any(target_os = "linux", feature = "local")))]
use tracing as _;

mod cli;

#[tokio::main]
async fn main() {
    if let Err(err) = cli::Cli::parse().run().await {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
