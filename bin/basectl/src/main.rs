//! Base infrastructure control CLI binary.

mod cli;

use basectl_cli::{MonitoringConfig, ViewId, run_app, run_flashblocks_json};
use clap::{CommandFactory, Parser};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = cli::Cli::parse();

    let config = &cli.config;
    let conductor_rpc = cli.conductor_rpc.clone();
    match cli.command {
        Some(cli::Commands::Monitor { command }) => {
            let view = command.map(|c| c.view_id()).unwrap_or(ViewId::Home);
            run_app(view, config, conductor_rpc).await
        }
        Some(cli::Commands::Flashblocks) => {
            run_flashblocks_json(MonitoringConfig::load(config).await?).await
        }
        None => {
            cli::Cli::command().print_help()?;
            Ok(())
        }
    }
}
