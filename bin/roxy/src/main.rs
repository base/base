//! Roxy binary entry point.

use anyhow::Result;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_roxy::{Config, Server};
use clap::Parser;
use tokio_util::sync::CancellationToken;

/// CLI entry point for the Roxy JSON-RPC reverse proxy.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Service configuration.
    #[command(flatten)]
    config: Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    cli.config.validate()?;

    LogConfig::default().init_tracing_subscriber().expect("failed to initialize tracing");

    let cancel = CancellationToken::new();
    let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

    let result = Server::serve(cli.config, cancel).await;
    signal_handle.abort();

    result
}
