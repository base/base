//! Base infrastructure control CLI binary.

mod cli;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    cli::Cli::parse().run().await
}
