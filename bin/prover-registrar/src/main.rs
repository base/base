#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use clap::Parser as _;

mod cli;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    if let Err(err) = cli::Cli::parse().run().await {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
