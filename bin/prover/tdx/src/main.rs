#![doc = include_str!("../README.md")]

use clap::Parser as _;

mod cli;

fn main() -> eyre::Result<()> {
    base_cli_utils::init_common!();
    cli::Cli::parse().run()
}
