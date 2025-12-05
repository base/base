//! Contains the CLI arguments

use std::path::PathBuf;

use base_reth_runner::{BaseNodeConfig, FlashblocksConfig, ProofsConfig, TracingConfig};
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
        }
    }
}
