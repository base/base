//! Base infrastructure control CLI binary.

mod cli;

use basectl_cli::{MonitoringConfig, ViewId, run_app, run_flashblocks_json};
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = cli::Cli::parse();

    let config = &cli.config;
    match cli.command {
        Some(cli::Commands::Config) => run_app(ViewId::Config, config).await,
        Some(cli::Commands::Flashblocks { json: true }) => {
            run_flashblocks_json(MonitoringConfig::load(config).await?).await
        }
        Some(cli::Commands::Flashblocks { json: false }) => {
            run_app(ViewId::Flashblocks, config).await
        }
        Some(cli::Commands::Da) => run_app(ViewId::DaMonitor, config).await,
        Some(cli::Commands::CommandCenter) => run_app(ViewId::CommandCenter, config).await,
        Some(cli::Commands::Conductor) => run_app(ViewId::Conductor, config).await,
        Some(cli::Commands::Upgrades) => run_app(ViewId::Upgrades, config).await,
        None => run_app(ViewId::Home, config).await,
    }
}
