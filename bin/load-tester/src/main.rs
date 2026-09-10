//! Base load tester binary entrypoint.

mod cli;

fn main() {
    base_common_cli_support::run_cli_main!(cli::Cli);
}
