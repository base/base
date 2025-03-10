//! Additional Node command arguments.
//!
//! Copied from OptimismNode to allow easy extension.

//! clap [Args](clap::Args) for optimism rollup configuration
use reth_optimism_node::args::RollupArgs;

use crate::tx_signer::Signer;

/// Parameters for rollup configuration
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct OpRbuilderArgs {
    /// Rollup configuration
    #[command(flatten)]
    pub rollup_args: RollupArgs,
    /// Builder secret key for signing last transaction in block
    #[arg(long = "rollup.builder-secret-key", env = "BUILDER_SECRET_KEY")]
    pub builder_signer: Option<Signer>,
    /// Websocket port for flashblock payload builder
    #[arg(
        long = "rollup.flashblocks-ws-url",
        env = "FLASHBLOCKS_WS_URL",
        default_value = "127.0.0.1:1111"
    )]
    pub flashblocks_ws_url: String,
}
