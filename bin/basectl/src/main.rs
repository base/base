//! Base infrastructure control CLI binary.

mod block;
mod cli;
mod confirm;
mod doctor;
mod p2p;
mod sync_status;

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
        Some(cli::Commands::Block { reference, json, raw }) => {
            block::run(MonitoringConfig::load(config).await?, &reference, json, raw).await
        }
        Some(cli::Commands::SyncStatus { el_rpc, cl_rpc, tip_tolerance, json, raw }) => {
            sync_status::run(
                MonitoringConfig::load(config).await?,
                el_rpc,
                cl_rpc,
                tip_tolerance,
                json,
                raw,
            )
            .await
        }
        Some(cli::Commands::P2p { command }) => {
            p2p::run(MonitoringConfig::load(config).await?, command).await
        }
        Some(cli::Commands::Doctor(args)) => {
            let has_failures = doctor::run(MonitoringConfig::load(config).await?, args).await?;
            if has_failures {
                std::process::exit(1);
            }
            Ok(())
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
