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
    #[arg(long, env = "OUTPUT_DIR", global = true)]
    pub(crate) output_dir: Option<PathBuf>,

    /// L1 chain ID for the devnet.
    #[arg(long, env = "L1_CHAIN_ID", global = true)]
    pub(crate) l1_chain_id: Option<u64>,

    /// L2 chain ID for the devnet.
    #[arg(long, env = "L2_CHAIN_ID", global = true)]
    pub(crate) l2_chain_id: Option<u64>,

    /// L1 beacon slot duration in seconds.
    #[arg(long, env = "SLOT_DURATION", global = true)]
    pub(crate) slot_duration: Option<u64>,

    /// Unix timestamp for genesis generation.
    #[arg(long, env = "GENESIS_TIME", global = true)]
    pub(crate) genesis_time: Option<u64>,

    /// Prefund balance for dev accounts.
    #[arg(long, env = "PREFUND_BALANCE", global = true)]
    pub(crate) prefund_balance: Option<String>,

    /// Base V1 activation block for L2 config patching.
    #[arg(long, env = "L2_BASE_V1_BLOCK", global = true)]
    pub(crate) l2_base_v1_block: Option<u64>,

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
        #[arg(long, env = "L1_RPC_URL")]
        l1_rpc: Option<String>,
    },
    /// Generate L2 configuration from a deployment.
    DeployL2 {
        /// Existing L1 RPC endpoint to deploy against.
        #[arg(long, env = "L1_RPC_URL")]
        l1_rpc: Option<String>,
    },
    /// Start a full local devnet.
    Devnet {
        /// Existing L1 RPC endpoint to reuse instead of starting a local L1.
        #[arg(long, env = "L1_RPC_URL")]
        l1_rpc: Option<String>,
    },
    /// Inspect the current devnet status.
    Status {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
}
