//! Load test binary entry point.

use anyhow::Result;
use clap::Parser;
use system_tests::{config, load, setup};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = config::Cli::parse();

    match cli.command {
        config::Commands::Setup(args) => setup::run(args).await,
        config::Commands::Load(args) => load::run(args).await,
    }
}
