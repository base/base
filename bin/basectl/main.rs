//! Base infrastructure control CLI binary.

use basectl_cli::{ChainConfig, ViewId, run_app, run_app_with_view};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
struct Cli {
    /// Chain configuration (mainnet, sepolia, devnet, or path to config file)
    #[arg(short = 'c', long = "config", default_value = "mainnet", global = true)]
    config: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Chain configuration operations
    #[command(visible_alias = "c")]
    Config,
    /// Flashblocks operations
    #[command(visible_alias = "f")]
    Flashblocks,
    /// DA (Data Availability) backlog monitor
    #[command(visible_alias = "d")]
    Da,
    /// Command center (combined view)
    #[command(visible_alias = "cc")]
    CommandCenter,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let cli = Cli::parse();

    let chain_config = ChainConfig::load(&cli.config).await?;

    match cli.command {
        Some(Commands::Config) => run_app_with_view(chain_config, ViewId::Config).await,
        Some(Commands::Flashblocks) => run_app_with_view(chain_config, ViewId::Flashblocks).await,
        Some(Commands::Da) => run_app_with_view(chain_config, ViewId::DaMonitor).await,
        Some(Commands::CommandCenter) => {
            run_app_with_view(chain_config, ViewId::CommandCenter).await
        }
        None => run_app(chain_config).await,
    }
}
