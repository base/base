//! Flashblocks CLI arguments.

use clap::Args;

/// Parameters for Flashblocks configuration.
///
/// The names in the struct are prefixed with `flashblocks` to avoid conflicts
/// with the legacy standard builder configuration (now removed) since these args are
/// flattened into the main `OpRbuilderArgs` struct with the other rollup/node args.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct FlashblocksArgs {
    /// Flashblocks is always enabled; these options tune its behavior.
    /// The port that we bind to for the websocket server that provides flashblocks
    #[arg(long = "flashblocks.port", env = "FLASHBLOCKS_WS_PORT", default_value = "1111")]
    pub flashblocks_port: u16,

    /// The address that we bind to for the websocket server that provides flashblocks
    #[arg(long = "flashblocks.addr", env = "FLASHBLOCKS_WS_ADDR", default_value = "127.0.0.1")]
    pub flashblocks_addr: String,

    /// flashblock block time in milliseconds
    #[arg(long = "flashblocks.block-time", default_value = "250", env = "FLASHBLOCK_BLOCK_TIME")]
    pub flashblocks_block_time: u64,

    /// Builder would always try to produce fixed number of flashblocks without regard to time of
    /// FCU arrival.
    /// In cases of late FCU it could lead to partially filled blocks.
    #[arg(long = "flashblocks.fixed", default_value = "false", env = "FLASHBLOCK_FIXED")]
    pub flashblocks_fixed: bool,

    /// Time by which blocks would be completed earlier in milliseconds.
    ///
    /// This time is used to account for latencies and would be deducted from total block
    /// building time before calculating number of fbs.
    #[arg(long = "flashblocks.leeway-time", default_value = "75", env = "FLASHBLOCK_LEEWAY_TIME")]
    pub flashblocks_leeway_time: u64,

    /// Whether to disable state root calculation for each flashblock
    #[arg(
        long = "flashblocks.disable-state-root",
        default_value = "false",
        env = "FLASHBLOCKS_DISABLE_STATE_ROOT"
    )]
    pub flashblocks_disable_state_root: bool,
}

impl Default for FlashblocksArgs {
    fn default() -> Self {
        Self {
            flashblocks_port: 1111,
            flashblocks_addr: "127.0.0.1".to_string(),
            flashblocks_block_time: 250,
            flashblocks_fixed: false,
            flashblocks_leeway_time: 75,
            flashblocks_disable_state_root: false,
        }
    }
}
