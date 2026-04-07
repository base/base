//! Base infrastructure control CLI binary.

mod cli;

use basectl_cli::{ChainConfig, ViewId, run_app, run_flashblocks_json};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = cli::Cli::parse();

    match cli.command {
        Some(cli::Commands::Config) => run_app(ViewId::Config).await,
        Some(cli::Commands::Flashblocks { json: true, network }) => {
            run_flashblocks_json(ChainConfig::load(&network).await?).await
        }
        Some(cli::Commands::Flashblocks { json: false, .. }) => run_app(ViewId::Flashblocks).await,
        Some(cli::Commands::Da) => run_app(ViewId::DaMonitor).await,
        Some(cli::Commands::CommandCenter) => run_app(ViewId::CommandCenter).await,
        Some(cli::Commands::Conductor) => run_app(ViewId::Conductor).await,
        None => run_app(ViewId::Home).await,
    }
}
