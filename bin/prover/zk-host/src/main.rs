#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use serde as _;

mod cli;

fn main() {
    base_cli_utils::run_cli_main!(cli::Cli);
}
