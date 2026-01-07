#![doc = include_str!("../README.md")]
#![doc(
    issue_tracker_base_url = "https://github.com/base/node-reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod cli;
pub mod commands;
pub mod flags;
pub mod version;

fn main() {
    use clap::Parser;

    base_cli::Backtracing::enable();
    base_cli::SigsegvHandler::install();

    if let Err(err) = cli::Cli::parse().run() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
