#![doc = include_str!("../README.md")]

mod cli;

fn main() {
    base_cli_utils::run_cli_main!(cli::Cli);
}
