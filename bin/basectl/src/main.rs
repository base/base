//! Base infrastructure control CLI binary.

mod conductor;
mod sequencer;

use basectl_cli::{Cli, Commands, MonitoringConfig, ViewId, run_app, run_flashblocks_json};
use clap::{CommandFactory, Parser};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = Cli::parse();

    // Install a tracing subscriber for CLI subcommands only. The TUI (monitor) is excluded
    // because a subscriber writing to stderr while ratatui holds the terminal corrupts the UI.
    if !matches!(cli.command, Some(Commands::Monitor { .. }) | None) {
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
        Some(Commands::Monitor { command }) => {
            let view = command.map(|c| c.view_id()).unwrap_or(ViewId::Home);
            run_app(view, config, conductor_rpc).await
        }
        Some(Commands::Block(command)) => command.run(MonitoringConfig::load(config).await?).await,
        Some(Commands::SyncStatus(command)) => {
            command.run(MonitoringConfig::load(config).await?).await
        }
        Some(Commands::P2p(command)) => {
            if command.run(MonitoringConfig::load(config).await?).await?.has_failures() {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Commands::Txpool(command)) => command.run(MonitoringConfig::load(config).await?).await,
        Some(Commands::Conductor { command }) => {
            if conductor::run(MonitoringConfig::load(config).await?, conductor_rpc, command)
                .await?
                .has_failures()
            {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Commands::Sequencer { command }) => {
            if sequencer::run(MonitoringConfig::load(config).await?, conductor_rpc, command)
                .await?
                .has_failures()
            {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Commands::Proofs(command)) => {
            if command.run(MonitoringConfig::load(config).await?).await?.has_failures() {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Commands::Doctor(command)) => {
            if command.run(MonitoringConfig::load(config).await?).await?.has_failures() {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Commands::Flashblocks) => {
            run_flashblocks_json(MonitoringConfig::load(config).await?).await
        }
        None => {
            Cli::command().print_help()?;
            Ok(())
        }
    }
}
