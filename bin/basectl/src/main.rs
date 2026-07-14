//! Base infrastructure control CLI binary.

mod block;
mod cli;
mod conductor;
mod confirm;
mod doctor;
mod helpers;
mod p2p;
mod proofs;
mod sequencer;
mod sync_status;
mod txpool;

use basectl_cli::{MonitoringConfig, ViewId, run_app, run_flashblocks_json};
use clap::{CommandFactory, Parser};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = cli::Cli::parse();

    // Install a tracing subscriber for CLI subcommands only. The TUI (monitor) is excluded
    // because a subscriber writing to stderr while ratatui holds the terminal corrupts the UI.
    if !matches!(cli.command, Some(cli::Commands::Monitor { .. }) | None) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_writer(std::io::stderr)
            .init();
    }

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
        Some(cli::Commands::Txpool { command }) => {
            txpool::run(MonitoringConfig::load(config).await?, command).await
        }
        Some(cli::Commands::Conductor { command }) => {
            if conductor::run(MonitoringConfig::load(config).await?, conductor_rpc, command)
                .await?
                .has_failures()
            {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(cli::Commands::Sequencer { command }) => {
            if sequencer::run(MonitoringConfig::load(config).await?, conductor_rpc, command)
                .await?
                .has_failures()
            {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(cli::Commands::Proofs { command }) => {
            if proofs::run(MonitoringConfig::load(config).await?, command).await?.has_failures() {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(cli::Commands::Doctor(args)) => {
            if doctor::run(MonitoringConfig::load(config).await?, args).await?.has_failures() {
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
