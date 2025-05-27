//! Additional Node command arguments.
//!
//! Copied from OptimismNode to allow easy extension.

//! clap [Args](clap::Args) for optimism rollup configuration
use std::path::PathBuf;

use crate::tx_signer::Signer;
use reth_optimism_node::args::RollupArgs;

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

    /// When set to true, the builder will build flashblocks
    /// and will build standard blocks at the chain block time.
    ///
    /// The default value will change in the future once the flashblocks
    /// feature is stable.
    #[arg(
        long = "rollup.enable-flashblocks",
        default_value = "false",
        env = "ENABLE_FLASHBLOCKS"
    )]
    pub enable_flashblocks: bool,

    /// Websocket port for flashblock payload builder
    #[arg(
        long = "rollup.flashblocks-ws-url",
        env = "FLASHBLOCKS_WS_URL",
        default_value = "127.0.0.1:1111"
    )]
    pub flashblocks_ws_url: String,
    /// chain block time in milliseconds
    #[arg(
        long = "rollup.chain-block-time",
        default_value = "1000",
        env = "CHAIN_BLOCK_TIME"
    )]
    pub chain_block_time: u64,
    /// flashblock block time in milliseconds
    #[arg(
        long = "rollup.flashblock-block-time",
        default_value = "250",
        env = "FLASHBLOCK_BLOCK_TIME"
    )]
    pub flashblock_block_time: u64,
    /// Signals whether to log pool transaction events
    #[arg(long = "builder.log-pool-transactions", default_value = "false")]
    pub log_pool_transactions: bool,

    /// How much time extra to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,
    /// Whether to enable revert protection by default
    #[arg(long = "builder.enable-revert-protection", default_value = "false")]
    pub enable_revert_protection: bool,

    #[arg(
        long = "builder.playground",
        num_args = 0..=1,
        default_missing_value = "$HOME/.playground/devnet/",
        value_parser = expand_path,
        env = "PLAYGROUND_DIR",
    )]
    pub playground: Option<PathBuf>,
}

fn expand_path(s: &str) -> Result<PathBuf, String> {
    shellexpand::full(s)
        .map_err(|e| format!("expansion error for `{s}`: {e}"))?
        .into_owned()
        .parse()
        .map_err(|e| format!("invalid path after expansion: {e}"))
}
