//! Base development network binary entrypoint.

use base_system_tests::DevnetCli;
use clap::Parser;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    DevnetCli::parse().run().await
}
