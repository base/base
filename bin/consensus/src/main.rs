#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Feature unification - enables `serde` and `reth-codec` features on
// `reth-optimism-primitives` for transitive dependencies.
use reth_optimism_primitives as _;

pub mod cli;
pub mod metrics;
pub mod version;

fn main() {
    use clap::Parser;

    base_cli_utils::Backtracing::enable();
    base_cli_utils::SigsegvHandler::install();

    if let Err(err) = cli::Cli::parse().run() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
