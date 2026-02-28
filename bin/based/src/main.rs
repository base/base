//! Based binary entry point.

mod cli;

use based::BasedConfig;
use clap::Parser;
use cli::Args;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = BasedConfig::from(Args::parse());
    based::run(config).await
}
