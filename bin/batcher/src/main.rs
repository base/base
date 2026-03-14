#![doc = include_str!("../README.md")]

use clap::Parser;

mod cli;

fn main() {
    base_cli_utils::init_common!();

    if let Err(err) = cli::Cli::parse().run() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
