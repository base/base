//! Run the library independently of the full node binary.

use base_genesis::{GenesisBuilder, GenesisCommand};
use clap::Parser;

#[derive(Debug, Parser)]
struct Command {
    #[command(flatten)]
    genesis: GenesisCommand,
}

fn main() -> eyre::Result<()> {
    GenesisBuilder::generate(Command::parse_from(std::env::args().skip(1)).genesis)
}
