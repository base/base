//! CLI arguments for the unified binary.

use base_cli_utils::GlobalArgs;
use clap::Args;
use reth_optimism_node::args::RollupArgs;

/// CLI arguments for the unified binary.
#[derive(Debug, Clone, Args)]
pub struct UnifiedArgs {
    /// Global logging and metrics arguments.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// Rollup arguments for the execution client.
    #[command(flatten)]
    pub rollup_args: RollupArgs,
}
