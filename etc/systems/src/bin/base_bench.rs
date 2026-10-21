//! Base snapshot benchmark binary entrypoint.

use base_system_tests::BenchmarkCli;
use clap::Parser;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    BenchmarkCli::parse().run().await
}
