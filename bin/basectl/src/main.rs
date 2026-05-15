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
    let conductor_rpc = cli.conductor_rpc.clone();
    match cli.command {
        Some(cli::Commands::Config) => run_app(ViewId::Config, config, conductor_rpc).await,
        Some(cli::Commands::Flashblocks { json: true }) => {
            run_flashblocks_json(MonitoringConfig::load(config).await?).await
        }
        Some(cli::Commands::Flashblocks { json: false }) => {
            run_app(ViewId::Flashblocks, config, conductor_rpc).await
        }
        Some(cli::Commands::Da) => run_app(ViewId::DaMonitor, config, conductor_rpc).await,
        Some(cli::Commands::CommandCenter) => {
            run_app(ViewId::CommandCenter, config, conductor_rpc).await
        }
        Some(cli::Commands::Conductor) => run_app(ViewId::Conductor, config, conductor_rpc).await,
        Some(cli::Commands::Upgrades) => run_app(ViewId::Upgrades, config, conductor_rpc).await,
        None => run_app(ViewId::Home, config, conductor_rpc).await,
    }
}
