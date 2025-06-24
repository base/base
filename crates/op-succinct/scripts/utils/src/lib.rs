use clap::Parser;
use std::path::PathBuf;

pub mod config_common;

/// The arguments for the host executable.
#[derive(Debug, Clone, Parser)]
pub struct HostExecutorArgs {
    /// The start block of the range to execute.
    #[arg(long)]
    pub start: Option<u64>,
    /// The end block of the range to execute.
    #[arg(long)]
    pub end: Option<u64>,
    /// The number of blocks to execute in a single batch.
    #[arg(long, default_value = "10")]
    pub batch_size: u64,
    /// Use cached witness generation.
    #[arg(long)]
    pub use_cache: bool,
    /// Use a fixed recent range.
    #[arg(long)]
    pub rolling: bool,
    /// The number of blocks to use for the default range.
    #[arg(long, default_value = "5")]
    pub default_range: u64,
    /// The environment file to use.
    #[arg(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Whether to generate proofs.
    #[arg(long)]
    pub prove: bool,
    /// Whether to fallback to timestamp-based L1 head estimation even though SafeDB is not
    /// activated for op-node.
    #[clap(long)]
    pub safe_db_fallback: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct ConfigArgs {
    /// The environment file to use.
    #[arg(long)]
    pub env_file: Option<PathBuf>,
}
