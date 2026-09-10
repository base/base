//! Base load tester binary entrypoint.

mod cli;

fn main() {
    base_common_cli::run_cli_main!(cli::Cli);
}
