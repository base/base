//! CLI definitions for `base-deployer`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Base devnet and deployment orchestration CLI.
#[derive(Debug, Parser)]
#[command(name = "base-deployer")]
#[command(about = "Generate genesis artifacts, deploy contracts, and run local Base devnets")]
pub(crate) struct Cli {
    /// Path to a JSON or TOML configuration file.
    #[arg(long, global = true)]
    pub(crate) config: Option<PathBuf>,

    /// Output directory for generated artifacts.
    #[arg(long, global = true)]
    pub(crate) output_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

/// Supported `base-deployer` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Generate L1 and L2 genesis artifacts for a devnet.
    Genesis,
    /// Deploy L1 contracts for a devnet.
    DeployL1 {
        /// Existing L1 RPC endpoint to deploy against.
        #[arg(long)]
        l1_rpc: Option<String>,
    },
    /// Generate L2 configuration from a deployment.
    DeployL2 {
        /// Existing L1 RPC endpoint to deploy against.
        #[arg(long)]
        l1_rpc: Option<String>,
    },
    /// Start a full local devnet.
    Devnet {
        /// Existing L1 RPC endpoint to reuse instead of starting a local L1.
        #[arg(long)]
        l1_rpc: Option<String>,
    },
    /// Inspect the current devnet status.
    Status {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
}
