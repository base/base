use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "load-test")]
#[command(about = "Load testing tool for TIPS ingress service", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Setup: Fund N wallets from a master wallet
    Setup(SetupArgs),
    /// Load: Run load test with funded wallets
    Load(LoadArgs),
}

#[derive(Parser)]
pub struct SetupArgs {
    /// Master wallet private key (must have funds)
    #[arg(long, env = "MASTER_KEY")]
    pub master_key: String,

    /// Sequencer RPC URL
    #[arg(long, env = "SEQUENCER_URL", default_value = "http://localhost:8547")]
    pub sequencer: String,

    /// Number of wallets to create and fund
    #[arg(long, default_value = "10")]
    pub num_wallets: usize,

    /// Amount of ETH to fund each wallet
    #[arg(long, default_value = "0.1")]
    pub fund_amount: f64,

    /// Output file for wallet data (required)
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Parser)]
pub struct LoadArgs {
    /// TIPS ingress RPC URL
    #[arg(long, env = "INGRESS_URL", default_value = "http://localhost:8080")]
    pub target: String,

    /// Sequencer RPC URL (for nonce fetching and receipt polling)
    #[arg(long, env = "SEQUENCER_URL", default_value = "http://localhost:8547")]
    pub sequencer: String,

    /// Path to wallets JSON file (required)
    #[arg(long)]
    pub wallets: PathBuf,

    /// Target transaction rate (transactions per second)
    #[arg(long, default_value = "100")]
    pub rate: u64,

    /// Test duration in seconds
    #[arg(long, default_value = "60")]
    pub duration: u64,

    /// Timeout for transaction inclusion (seconds)
    #[arg(long, default_value = "60")]
    pub tx_timeout: u64,

    /// Random seed for reproducibility
    #[arg(long)]
    pub seed: Option<u64>,

    /// Output file for metrics (JSON)
    #[arg(long)]
    pub output: Option<PathBuf>,
}
