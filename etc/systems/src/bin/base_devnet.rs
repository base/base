//! Base development network binary entrypoint.

use base_system_tests::{DevnetCli, DevnetCommand};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = DevnetCli::parse();
    // Keep stdout reserved for the inspector's single JSON object. Its logs default to off because
    // provider logs can echo RPC URLs that carry credentials.
    let inspecting = matches!(cli.command, DevnetCommand::InspectSnapshot(_));
    let default_filter = if inspecting { "off" } else { "info" };
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
    );
    if inspecting {
        subscriber.with_writer(std::io::stderr).init();
    } else {
        subscriber.init();
    }

    cli.run().await
}
