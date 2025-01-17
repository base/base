use clap::Parser;
use std::path::PathBuf;

/// The arguments for the host executable.
#[derive(Debug, Clone, Parser)]
pub struct HostExecutorArgs {
    /// The start block of the range to execute.
    #[clap(long)]
    pub start: Option<u64>,
    /// The end block of the range to execute.
    #[clap(long)]
    pub end: Option<u64>,
    /// The number of blocks to execute in a single batch.
    #[clap(long, default_value = "10")]
    pub batch_size: u64,
    /// Use cached witness generation.
    #[clap(long)]
    pub use_cache: bool,
    /// Use a fixed recent range.
    #[clap(long)]
    pub rolling: bool,
    /// The number of blocks to use for the default range.
    #[clap(long, default_value = "5")]
    pub default_range: u64,
    /// The environment file to use.
    #[clap(long, default_value = ".env")]
    pub env_file: PathBuf,
    /// Whether to generate proofs.
    #[clap(long)]
    pub prove: bool,
}
