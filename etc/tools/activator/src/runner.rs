use eyre::Result;

use crate::{CalldataEncoder, Cli, Commands, OutputWriter, PrecompileCatalog, StatusChecker};

/// Runs activator commands.
#[derive(Debug, Clone, Copy)]
pub struct Activator;

impl Activator {
    /// Executes one parsed activator command.
    pub async fn run(cli: Cli) -> Result<()> {
        match cli.command {
            Commands::List(command) => {
                OutputWriter::write_inventory(command.format, &PrecompileCatalog::beryl())
            }
            Commands::Calldata(command) => {
                let output = CalldataEncoder::encode(command.action, command.feature);
                OutputWriter::write_calldata(command.format, &output)
            }
            Commands::Status(command) => {
                let networks = command.networks();
                let report = StatusChecker::check_all(&networks).await?;
                OutputWriter::write_status(command.format, &report)
            }
        }
    }
}
