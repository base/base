//! `base-da-server` binary: alt-DA HTTP service for L3 batch data.

mod cli;

fn main() {
    base_cli_utils::run_cli_main!(cli::Cli);
}
