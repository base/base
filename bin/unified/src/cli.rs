//! CLI argument definitions for the unified binary.

use std::path::PathBuf;

use clap::Parser;
use reth_optimism_node::args::RollupArgs;

/// Unified CL+EL binary for Base.
///
/// Runs both the consensus layer (kona) and execution layer (reth) in a single
/// process with direct in-process channel communication (no HTTP or IPC overhead).
#[derive(Parser, Debug)]
#[command(name = "base-unified")]
#[command(about = "Base unified CL+EL node")]
pub(crate) struct UnifiedCli {
    /// Path to the rollup configuration file (kona format).
    #[arg(long, value_name = "FILE")]
    pub rollup_config: Option<PathBuf>,

    /// Path to the chain specification file.
    #[arg(long, value_name = "FILE")]
    pub chain: Option<PathBuf>,

    /// Data directory for both CL and EL.
    #[arg(long, value_name = "DIR")]
    pub datadir: Option<PathBuf>,

    /// L1 RPC endpoint URL.
    #[arg(long, value_name = "URL")]
    pub l1_rpc_url: Option<String>,

    /// L1 beacon API endpoint URL.
    #[arg(long, value_name = "URL")]
    pub l1_beacon_url: Option<String>,

    /// Rollup-specific arguments (forwarded to reth OP node).
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// Log verbosity level.
    #[arg(short, long, default_value = "info")]
    pub verbosity: String,

    /// Skip launching the consensus layer (for testing EL only).
    #[arg(long)]
    pub el_only: bool,
}
