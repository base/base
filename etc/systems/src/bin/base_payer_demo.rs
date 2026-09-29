//! ERC-8168 token-payer devnet demo entrypoint.

use base_system_tests::PayerDemoCli;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    PayerDemoCli::parse().run().await
}
