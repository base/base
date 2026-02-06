use basectl_cli::{
    commands::{
        config::{ConfigCommand, default_view, run_config},
        flashblocks::{FlashblocksCommand, default_subscribe, run_flashblocks},
    },
    config::ChainConfig,
    tui::{HomeSelection, NavResult, run_homescreen},
};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
struct Cli {
    /// Chain configuration (mainnet, sepolia, or path to config file)
    #[arg(short = 'c', long = "config", default_value = "mainnet", global = true)]
    config: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Chain configuration operations
    #[command(visible_alias = "c")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Flashblocks operations
    #[command(visible_alias = "f")]
    Flashblocks {
        #[command(subcommand)]
        command: FlashblocksCommand,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let chain_config = ChainConfig::load(&cli.config)?;

    match cli.command {
        Some(Commands::Config { command }) => run_config(command, &chain_config).await,
        Some(Commands::Flashblocks { command }) => run_flashblocks(command, &chain_config).await,
        None => {
            // Show homescreen when no command provided
            loop {
                let next = match run_homescreen()? {
                    HomeSelection::Config => default_view(&chain_config).await?,
                    HomeSelection::Flashblocks => default_subscribe(&chain_config).await?,
                    HomeSelection::Quit => return Ok(()),
                };
                if next == NavResult::Quit {
                    return Ok(());
                }
            }
        }
    }
}
