#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod cli;
pub mod metrics;

fn main() {
    use clap::Parser;
    use tracing::error;

    base_cli_utils::init_common!();

    if let Err(err) = cli::Cli::parse().run() {
        error!(error = ?err, "consensus node failed");
        std::process::exit(1);
    }
}
