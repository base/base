//! Proposer binary entry point.

use base_proposer::{Cli, ProposerConfig};
use clap::Parser;
use eyre::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Validate configuration
    let config = ProposerConfig::from_cli(cli)?;

    // Initialize tracing with base-cli-utils
    config.log.init_tracing_subscriber()?;

    info!(version = env!("CARGO_PKG_VERSION"), "Proposer starting");
    info!(
        poll_interval = ?config.poll_interval,
        min_proposal_interval = config.min_proposal_interval,
        allow_non_finalized = config.allow_non_finalized,
        "Configuration loaded"
    );

    // TODO: Initialize driver and start main loop

    Ok(())
}
