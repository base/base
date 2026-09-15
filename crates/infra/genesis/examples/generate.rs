//! Run the library independently of the full node binary.

use base_genesis::{GenesisBuilder, GenesisCommand};
use clap::Parser;

fn main() -> eyre::Result<()> {
    GenesisBuilder::generate(GenesisCommand::parse_from(std::env::args().skip(1)))
}
