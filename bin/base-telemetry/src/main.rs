//! Base telemetry service binary entry point.

use anyhow::Result;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_telemetry_service::{BaseTelemetryServer, ServerConfig};
use clap::Parser;
use tokio_util::sync::CancellationToken;

base_cli_utils::define_log_args!("BASE_TELEMETRY");

/// CLI entry point for the Base telemetry service.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Service configuration.
    #[command(flatten)]
    config: ServerConfig,
    /// Logging configuration.
    #[command(flatten)]
    log: LogArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    LogConfig::from(cli.log).init_tracing_subscriber().expect("failed to initialize tracing");

    let cancel = CancellationToken::new();
    let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

    let result = BaseTelemetryServer::serve(cli.config, cancel).await;
    signal_handle.abort();

    result
}
