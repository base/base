//! CLI argument parsing for the Base Batcher binary.

use std::time::Duration;

use base_batcher_core::ThrottleConfig;
use base_batcher_service::{BatcherConfig, BatcherService, SecretKey};
use base_cli_utils::{LogConfig, RuntimeManager};
use clap::{Args, Parser};
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

base_cli_utils::define_log_args!("BATCHER");
base_cli_utils::define_metrics_args!("BATCHER", 7300);

/// The Base Batcher CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    about = "Base Batcher — submits L2 batch data to L1",
    long_about = None
)]
pub(crate) struct Cli {
    /// Batcher arguments.
    #[command(flatten)]
    pub(crate) args: BatcherArgs,
}

impl Cli {
    /// Run the batcher CLI.
    pub(crate) fn run(self) -> eyre::Result<()> {
        LogConfig::from(self.args.logging.clone()).init_tracing_subscriber()?;
        RuntimeManager::run_until_ctrl_c(self.args.exec())
    }
}

/// CLI arguments for the batcher.
#[derive(Args, Clone, Debug)]
pub(crate) struct BatcherArgs {
    /// L1 RPC endpoint.
    #[arg(long = "l1-rpc-url", env = "BATCHER_L1_RPC_URL")]
    pub l1_rpc_url: Url,

    /// L2 HTTP RPC endpoint (used for all JSON-RPC calls including throttle control).
    #[arg(long = "l2-rpc-url", env = "BATCHER_L2_RPC_URL")]
    pub l2_rpc_url: Url,

    /// Optional L2 WebSocket endpoint for new-block subscriptions.
    ///
    /// When provided, the batcher subscribes to new block headers over this
    /// WebSocket connection. Without it, polling is used exclusively.
    #[arg(long = "l2-ws-url", env = "BATCHER_L2_WS_URL")]
    pub l2_ws_url: Option<Url>,

    /// Rollup node RPC endpoint.
    #[arg(long = "rollup-rpc-url", env = "BATCHER_ROLLUP_RPC_URL")]
    pub rollup_rpc_url: Url,

    /// Batcher private key (hex-encoded 32-byte secret).
    #[arg(long = "private-key", env = "BATCHER_PRIVATE_KEY")]
    pub private_key: SecretKey,

    /// L2 block polling interval in seconds.
    #[arg(long = "poll-interval", default_value = "1", env = "BATCHER_POLL_INTERVAL")]
    pub poll_interval_secs: u64,

    /// Maximum L1 blocks a channel may stay open.
    #[arg(
        long = "max-channel-duration",
        default_value = "2",
        env = "BATCHER_MAX_CHANNEL_DURATION"
    )]
    pub max_channel_duration: u64,

    /// Safety margin for channel timeout.
    #[arg(long = "sub-safety-margin", default_value = "0", env = "BATCHER_SUB_SAFETY_MARGIN")]
    pub sub_safety_margin: u64,

    /// Target compressed frame size in bytes.
    #[arg(long = "target-frame-size", default_value = "130044", env = "BATCHER_TARGET_FRAME_SIZE")]
    pub target_frame_size: usize,

    /// Number of frames (blobs) per L1 transaction.
    #[arg(long = "target-num-frames", default_value = "1", env = "BATCHER_TARGET_NUM_FRAMES")]
    pub target_num_frames: usize,

    /// Maximum number of in-flight (unconfirmed) transactions.
    #[arg(
        long = "max-pending-transactions",
        default_value = "1",
        env = "BATCHER_MAX_PENDING_TRANSACTIONS"
    )]
    pub max_pending_transactions: usize,

    /// Number of L1 confirmations before a tx is considered finalized.
    #[arg(long = "num-confirmations", default_value = "1", env = "BATCHER_NUM_CONFIRMATIONS")]
    pub num_confirmations: usize,

    /// Timeout before resubmitting a transaction (seconds).
    #[arg(
        long = "resubmission-timeout",
        default_value = "48",
        env = "BATCHER_RESUBMISSION_TIMEOUT"
    )]
    pub resubmission_timeout_secs: u64,

    /// DA backlog threshold in bytes at which throttling activates.
    ///
    /// When the estimated unsubmitted DA backlog exceeds this value, the batcher
    /// signals the sequencer to reduce block throughput. Matches op-batcher's
    /// `--throttle-threshold` default of 1 MB.
    #[arg(
        long = "throttle-threshold",
        default_value = "1000000",
        env = "BATCHER_THROTTLE_THRESHOLD"
    )]
    pub throttle_threshold: u64,

    /// Disable DA throttling.
    ///
    /// By default throttling is enabled (matching op-batcher behaviour). Pass
    /// this flag to submit batches at full rate regardless of DA backlog.
    #[arg(long = "no-throttle", env = "BATCHER_NO_THROTTLE")]
    pub no_throttle: bool,

    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration.
    #[command(flatten)]
    pub metrics: MetricsArgs,
}

impl BatcherArgs {
    /// Convert CLI arguments into a [`BatcherConfig`].
    fn into_config(self) -> eyre::Result<BatcherConfig> {
        let encoder_config = base_batcher_encoder::EncoderConfig {
            target_frame_size: self.target_frame_size,
            max_frame_size: self.target_frame_size,
            max_channel_duration: self.max_channel_duration,
            sub_safety_margin: self.sub_safety_margin,
            target_num_frames: self.target_num_frames,
        };
        encoder_config.validate()?;
        Ok(BatcherConfig {
            l1_rpc_url: self.l1_rpc_url,
            l2_rpc_url: self.l2_rpc_url,
            l2_ws_url: self.l2_ws_url,
            rollup_rpc_url: self.rollup_rpc_url,
            batcher_private_key: self.private_key,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            encoder_config,
            max_pending_transactions: self.max_pending_transactions,
            num_confirmations: self.num_confirmations,
            resubmission_timeout: Duration::from_secs(self.resubmission_timeout_secs),
            throttle: if self.no_throttle {
                None
            } else {
                Some(ThrottleConfig {
                    threshold_bytes: self.throttle_threshold,
                    max_intensity: 1.0,
                    ..Default::default()
                })
            },
            metrics_port: self.metrics.port,
        })
    }

    /// Execute the batcher.
    async fn exec(self) -> eyre::Result<()> {
        let config = self.into_config()?;
        info!(
            l1_rpc = %config.l1_rpc_url,
            l2_rpc = %config.l2_rpc_url,
            "batcher configured"
        );

        let cancellation = CancellationToken::new();
        let _signal_handle = RuntimeManager::install_signal_handler(cancellation.clone());

        let service = BatcherService::new(config);
        service.start(cancellation).await
    }
}
