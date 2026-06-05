#![doc = include_str!("../README.md")]

use base_activator::{Activator, Cli};
use clap::Parser;
use eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    Activator::run(Cli::parse()).await
}
