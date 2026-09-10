//! Base snapshot benchmark binary entrypoint.

use base_testing_devnet::BenchmarkCli;
use clap::Parser;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    BenchmarkCli::parse().run().await
}
