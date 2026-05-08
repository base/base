//! Base load tester binary entrypoint.

use clap::Parser as _;

mod cli;

fn main() {
    base_cli_utils::init_common!();

    if let Err(err) = cli::Cli::parse().run() {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
