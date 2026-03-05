//! Contains the CLI arguments

use base_flashblocks::FlashblocksConfig;
use base_node_core::args::RollupArgs;
use url::Url;

/// CLI Arguments
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup arguments
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// A URL pointing to a secure websocket subscription that streams out flashblocks.
    ///
    /// If given, the flashblocks are received to build pending block. All request with "pending"
    /// block tag will use the pending state based on flashblocks.
    #[arg(long, alias = "websocket-url")]
    pub flashblocks_url: Option<Url>,

    /// The max pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,

    /// Enable flashblocks cached execution in the engine validator.
    ///
    /// This is disabled by default until cached execution has been further validated in
    /// production conditions.
    #[arg(
        long = "enable-flashblocks-cached-execution",
        env = "BASE_NODE_ENABLE_FLASHBLOCKS_CACHED_EXECUTION",
        default_value = "false",
        value_name = "ENABLE_FLASHBLOCKS_CACHED_EXECUTION"
    )]
    pub enable_flashblocks_cached_execution: bool,
}

impl From<&Args> for Option<FlashblocksConfig> {
    fn from(args: &Args) -> Self {
        args.flashblocks_url.clone().map(|url| {
            FlashblocksConfig::new(url, args.max_pending_blocks_depth)
                .with_cached_execution_enabled(args.enable_flashblocks_cached_execution)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with_flashblocks_url() -> Args {
        Args {
            rollup_args: RollupArgs::default(),
            flashblocks_url: Some(Url::parse("ws://localhost:12345").unwrap()),
            max_pending_blocks_depth: 3,
            enable_transaction_tracing: false,
            enable_transaction_tracing_logs: false,
            enable_metering: false,
            enable_flashblocks_cached_execution: false,
        }
    }

    #[test]
    fn cached_execution_flag_defaults_to_disabled() {
        let args = args_with_flashblocks_url();
        let config: Option<FlashblocksConfig> = (&args).into();
        let config = config.expect("flashblocks config should be created");
        assert!(!config.cached_execution_enabled);
    }

    #[test]
    fn cached_execution_flag_maps_to_flashblocks_config() {
        let mut args = args_with_flashblocks_url();
        args.enable_flashblocks_cached_execution = true;

        let config: Option<FlashblocksConfig> = (&args).into();
        let config = config.expect("flashblocks config should be created");
        assert!(config.cached_execution_enabled);
    }
}
