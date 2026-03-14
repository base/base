//! CLI argument parsing for the Base Batcher binary.

use std::time::Duration;

use alloy_primitives::Address;
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

    /// L2 RPC endpoint.
    #[arg(long = "l2-rpc-url", env = "BATCHER_L2_RPC_URL")]
    pub l2_rpc_url: Url,

    /// Rollup node RPC endpoint.
    #[arg(long = "rollup-rpc-url", env = "BATCHER_ROLLUP_RPC_URL")]
    pub rollup_rpc_url: Url,

    /// Batcher private key (hex-encoded 32-byte secret).
    #[arg(long = "private-key", env = "BATCHER_PRIVATE_KEY")]
    pub private_key: SecretKey,

    /// Batcher inbox address on L1.
    #[arg(long = "inbox-address", env = "BATCHER_INBOX_ADDRESS")]
    pub inbox_address: Address,

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
    #[arg(long = "sub-safety-margin", default_value = "4", env = "BATCHER_SUB_SAFETY_MARGIN")]
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
        Ok(BatcherConfig {
            l1_rpc_url: self.l1_rpc_url,
            l2_rpc_url: self.l2_rpc_url,
            rollup_rpc_url: self.rollup_rpc_url,
            batcher_private_key: self.private_key,
            inbox_address: self.inbox_address,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            encoder_config: base_batcher_encoder::EncoderConfig {
                target_frame_size: self.target_frame_size,
                max_frame_size: self.target_frame_size,
                max_channel_duration: self.max_channel_duration,
                sub_safety_margin: self.sub_safety_margin,
                target_num_frames: self.target_num_frames,
            },
            max_pending_transactions: self.max_pending_transactions,
            num_confirmations: self.num_confirmations,
            resubmission_timeout: Duration::from_secs(self.resubmission_timeout_secs),
            throttle: None,
            metrics_port: self.metrics.port,
        })
    }

    /// Execute the batcher.
    async fn exec(self) -> eyre::Result<()> {
        let config = self.into_config()?;
        info!(
            inbox = %config.inbox_address,
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
