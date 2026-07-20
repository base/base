#![doc = include_str!("../README.md")]

use base_state_populate::{Args, Populator, SubCommand, Verifier};
use clap::Parser;
use eyre::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let args = Args::parse();
    match args.command {
        SubCommand::Populate(args) => Populator::run(args),
        SubCommand::Verify(args) => Verifier::run(args),
    }
}
