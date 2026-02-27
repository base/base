//! Base infrastructure control CLI binary.

mod cli;

use basectl_cli::{ChainConfig, ViewId, run_app, run_app_with_view, run_flashblocks_json};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = cli::Cli::parse();
    let chain_config = ChainConfig::load(&cli.config).await?;

    match cli.command {
        Some(cli::Commands::Config) => run_app_with_view(chain_config, ViewId::Config).await,
        Some(cli::Commands::Flashblocks { json: true }) => run_flashblocks_json(chain_config).await,
        Some(cli::Commands::Flashblocks { json: false }) => {
            run_app_with_view(chain_config, ViewId::Flashblocks).await
        }
        Some(cli::Commands::Da) => run_app_with_view(chain_config, ViewId::DaMonitor).await,
        Some(cli::Commands::CommandCenter) => {
            run_app_with_view(chain_config, ViewId::CommandCenter).await
        }
        None => run_app(chain_config).await,
    }
}
