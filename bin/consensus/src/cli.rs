//! Contains the CLI entry point for the Base consensus binary.

use base_cli_utils::GlobalArgs;
use clap::Parser;

use crate::version;

/// The Base Consensus CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = version::SHORT_VERSION,
    long_version = version::LONG_VERSION,
    about,
    long_about = None
)]
pub struct Cli {
    /// Global arguments for the Base Consensus CLI.
    #[command(flatten)]
    pub global: GlobalArgs,
}

impl Cli {
    /// Parse the CLI arguments.
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    /// Run the CLI.
    pub fn run(self) -> eyre::Result<()> {
        // TODO: Implement the CLI logic
        Ok(())
    }
}
