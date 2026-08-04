#![doc = include_str!("../README.md")]

use basectl_cli::Cli;
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install default rustls CryptoProvider"))?;

    let cli = Cli::parse();

    // Install a tracing subscriber for CLI subcommands only. The TUI (monitor) is excluded
    // because a subscriber writing to stderr while ratatui holds the terminal corrupts the UI.
    if !cli.is_tui() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    if cli.run().await?.has_failures() {
        std::process::exit(1);
    }
    Ok(())
}
