#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;

#[tokio::main]
async fn main() {
    base_cli_utils::run_cli_main!(async cli::Cli);
}
