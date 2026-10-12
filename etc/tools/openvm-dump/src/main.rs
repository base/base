#![doc = include_str!("../README.md")]

use base_openvm_dump::OpenVmRangeDump;
use clap::Parser;
use eyre::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // Derivation logs one WARN per historical batch; a live zeronet
            // block will emit millions of "Dropping old batch" lines.
            EnvFilter::new("info,batch_validator=off,batch_queue=off")
        }))
        .try_init();

    OpenVmRangeDump::parse().dump().await
}
