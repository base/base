//! Entry point for offline mainnet component benchmarks.

use base_replay::Cli;
use clap::Parser;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    Cli::parse().run().await
}
