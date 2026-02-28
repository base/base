//! Contains the CLI arguments for the basectl binary.

use basectl_cli::{BasectlCommand, BasectlConfig, ChainConfig};
use clap::{Parser, Subcommand};

/// Base infrastructure control CLI.
#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
pub(crate) struct Cli {
    /// Chain configuration (mainnet, sepolia, devnet, or path to config file)
    #[arg(short = 'c', long = "config", default_value = "mainnet", global = true)]
    pub(crate) config: String,

    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

/// Subcommands for the basectl CLI.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Chain configuration operations
    #[command(visible_alias = "c")]
    Config,
    /// Flashblocks operations
    #[command(visible_alias = "f")]
    Flashblocks {
        /// Output flashblocks as JSON lines instead of the TUI
        #[arg(long)]
        json: bool,
    },
    /// DA (Data Availability) backlog monitor
    #[command(visible_alias = "d")]
    Da,
    /// Command center (combined view)
    #[command(visible_alias = "cc")]
    CommandCenter,
}

impl Cli {
    /// Converts CLI arguments into a [`BasectlConfig`] and runs the application.
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        let config = self.into_config().await?;
        config.run().await
    }

    /// Converts CLI arguments into a [`BasectlConfig`].
    async fn into_config(self) -> anyhow::Result<BasectlConfig> {
        let chain_config = ChainConfig::load(&self.config).await?;

        let command = match self.command {
            Some(Commands::Config) => BasectlCommand::Config,
            Some(Commands::Flashblocks { json }) => BasectlCommand::Flashblocks { json },
            Some(Commands::Da) => BasectlCommand::Da,
            Some(Commands::CommandCenter) => BasectlCommand::CommandCenter,
            None => BasectlCommand::Default,
        };

        Ok(BasectlConfig { chain_config, command })
    }
}
