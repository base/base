//! Contains the CLI arguments

use base_flashblocks::FlashblocksConfig;
use base_node_core::args::RollupArgs;
use base_tx_forwarding::TxForwardingConfig;
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

    /// Enable cached execution via the flashblocks-aware engine validator.
    #[arg(long = "flashblocks.cached-execution", requires = "flashblocks_url")]
    pub flashblocks_cached_execution: bool,

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

    /// Enable transaction forwarding for mempool nodes to builder RPC endpoints
    #[arg(long = "enable-tx-forwarding", value_name = "ENABLE_TX_FORWARDING")]
    pub enable_tx_forwarding: bool,

    /// Builder RPC endpoints for transaction forwarding (one forwarder per URL), used by mempool nodes
    #[arg(
        long = "builder-rpc-urls",
        value_name = "BUILDER_RPC_URLS",
        requires = "enable-tx-forwarding"
    )]
    pub builder_rpc_urls: Vec<Url>,

    /// Resend transactions that haven't been included after this duration in ms (default: 2 blocks)
    #[arg(
        long = "tx-forwarding-resend-after-ms",
        value_name = "TX_FORWARDING_RESEND_AFTER_MS",
        default_value = "4000",
        requires = "enable-tx-forwarding"
    )]
    pub tx_forwarding_resend_after_ms: u64,

    /// Maximum number of transactions per forwarding batch
    #[arg(
        long = "tx-forwarding-batch-size",
        value_name = "TX_FORWARDING_BATCH_SIZE",
        default_value = "100",
        requires = "enable-tx-forwarding"
    )]
    pub tx_forwarding_batch_size: usize,

    /// Maximum time to wait before sending an incomplete batch in ms
    #[arg(
        long = "tx-forwarding-batch-timeout-ms",
        value_name = "TX_FORWARDING_BATCH_TIMEOUT_MS",
        default_value = "50",
        requires = "enable-tx-forwarding"
    )]
    pub tx_forwarding_batch_timeout_ms: u64,
}

impl From<&Args> for Option<FlashblocksConfig> {
    fn from(args: &Args) -> Self {
        args.flashblocks_url.clone().map(|url| {
            let mut config = FlashblocksConfig::new(url, args.max_pending_blocks_depth);
            config.cached_execution = args.flashblocks_cached_execution;
            config
        })
    }
}

impl From<&Args> for TxForwardingConfig {
    fn from(args: &Args) -> Self {
        if !args.enable_tx_forwarding || args.builder_rpc_urls.is_empty() {
            return Self::default();
        }

        Self::new(args.builder_rpc_urls.clone())
            .with_resend_after_ms(args.tx_forwarding_resend_after_ms)
            .with_max_batch_size(args.tx_forwarding_batch_size)
    }
}
