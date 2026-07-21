#![doc = include_str!("../README.md")]

use base_b20_state_populate::{Args, Populator, SubCommand, Verifier};
use clap::Parser;
use eyre::Result;
use tracing_subscriber::EnvFilter;

/// Entry point for the b20-state-populate binary.
fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let args = Args::parse();
    match args.command {
        SubCommand::Populate(a) => Populator::run(a),
        SubCommand::Verify(a) => Verifier::run(a),
    }
}
