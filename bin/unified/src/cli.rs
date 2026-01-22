//! Contains the CLI entry point for the Base unified binary.

use base_cli_utils::{CliStyles, GlobalArgs};
use clap::Parser;

use crate::version;

/// The Base Consensus CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = version::SHORT_VERSION,
    long_version = version::LONG_VERSION,
    styles = CliStyles::init(),
    about,
    long_about = None
)]
pub struct Cli {
    /// Global arguments for the Base Consensus CLI.
    #[command(flatten)]
    pub global: GlobalArgs,
}

impl Cli {
    /// Runs the CLI.
    pub fn run(self) -> eyre::Result<()> {
        println!("Base Unified CLI");
        Err(eyre::eyre!("Not yet implemented"))
    }
}
