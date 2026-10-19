#![doc = include_str!("../README.md")]

use base_withdrawer::{Withdrawer, WithdrawerArgs, WithdrawerConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = WithdrawerArgs::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| eyre::eyre!(error))?;
    Withdrawer::new(WithdrawerConfig::try_from(args)?).run().await?;
    Ok(())
}
