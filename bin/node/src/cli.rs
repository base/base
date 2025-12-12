//! Contains the CLI arguments

use std::{path::PathBuf, sync::Arc, time::Duration};

use base_reth_runner::{BaseNodeConfig, FlashblocksConfig, ProofsConfig, TracingConfig};
use once_cell::sync::OnceCell;
use reth_optimism_node::args::RollupArgs;

/// CLI Arguments
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup arguments
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// The websocket url used for flashblocks.
    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    /// The mac pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing ExEx for mempool-to-block timing analysis
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

    /// If true, initialize external-proofs exex to save and serve trie nodes to provide proofs
    /// faster.
    #[arg(long = "proofs-history", value_name = "PROOFS_HISTORY", default_value = "false")]
    pub proofs_history: bool,

    /// The path to the storage DB for proofs history.
    #[arg(
        long = "proofs-history.storage-path",
        value_name = "PROOFS_HISTORY_STORAGE_PATH",
        required_if_eq("proofs_history", "true")
    )]
    pub proofs_history_storage_path: Option<PathBuf>,

    /// The window to span blocks for proofs history. Value is the number of blocks.
    /// Default is 1 month of blocks based on 2 seconds block time.
    /// 30 * 24 * 60 * 60 / 2 = `1_296_000`
    // TODO: Pass this arg to the ExEx or remove it if not needed.
    #[arg(
        long = "proofs-history.window",
        default_value_t = 1_296_000,
        value_name = "PROOFS_HISTORY_WINDOW"
    )]
    pub proofs_history_window: u64,

    /// Interval between proof-storage prune runs. Accepts human-friendly durations
    /// like "100s", "5m", "1h". Defaults to 1h.
    ///
    /// - Shorter intervals prune smaller batches more often, so each prune run tends to be faster
    ///   and the blocking pause for writes is shorter, at the cost of more frequent pauses.
    /// - Longer intervals prune larger batches less often, which reduces how often pruning runs,
    ///   but each run can take longer and block writes for longer.
    ///
    /// A shorter interval is preferred so that prune
    /// runs stay small and don’t stall writes for too long.
    ///
    /// CLI: `--proofs-history.prune-interval 10m`
    #[arg(
        long = "proofs-history.prune-interval",
        value_name = "PROOFS_HISTORY_PRUNE_INTERVAL",
        default_value = "1h",
        value_parser = humantime::parse_duration
    )]
    pub proofs_history_prune_interval: Duration,
}

impl Args {
    /// Returns if flashblocks is enabled.
    /// If the websocket url is specified through the CLI.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }

    /// Returns the [`FlashblocksConfig`] if flashblocks is enabled.
    pub fn flashblocks_config(&self) -> Option<FlashblocksConfig> {
        self.websocket_url.as_ref().map(|websocket_url| FlashblocksConfig {
            websocket_url: websocket_url.clone(),
            max_pending_blocks_depth: self.max_pending_blocks_depth,
        })
    }

    /// Returns the [`ProofsConfig`].
    pub fn proofs_config(&self) -> ProofsConfig {
        ProofsConfig {
            enabled: self.proofs_history,
            storage_path: self.proofs_history_storage_path.clone(),
        }
    }

    /// Returns the [`TracingConfig`].
    pub const fn tracing_config(&self) -> TracingConfig {
        TracingConfig {
            enabled: self.enable_transaction_tracing,
            logs_enabled: self.enable_transaction_tracing_logs,
        }
    }
}

impl From<Args> for BaseNodeConfig {
    fn from(args: Args) -> Self {
        Self {
            flashblocks: args.flashblocks_config(),
            proofs: args.proofs_config(),
            tracing: args.tracing_config(),
            rollup_args: args.rollup_args,
            metering_enabled: args.enable_metering,
            flashblocks_cell: Arc::new(OnceCell::new()),
        }
    }
}
