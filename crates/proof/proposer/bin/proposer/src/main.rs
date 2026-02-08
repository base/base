//! Proposer binary entry point.

use base_proposer::Cli;
use clap::Parser;
use eyre::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with JSON support when RUST_LOG_FORMAT=json
    let json_format = std::env::var("RUST_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if json_format {
        tracing_subscriber::registry()
            .with(fmt::layer().json())
            .with(EnvFilter::from_default_env())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(fmt::layer())
            .with(EnvFilter::from_default_env())
            .init();
    }

    // Parse CLI
    let cli = Cli::parse();

    info!(version = env!("CARGO_PKG_VERSION"), "Proposer initialized");
    info!(poll_interval = ?cli.poll_interval, "Configuration loaded");

    Ok(())
}
