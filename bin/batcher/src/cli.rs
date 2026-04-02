//! CLI argument parsing for the Base Batcher binary.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use alloy_signer_local::PrivateKeySigner;
use base_batcher_core::ThrottleConfig;
use base_batcher_service::{BatcherConfig, BatcherService};
use base_cli_utils::{LogConfig, RuntimeManager};
use base_runtime::TokioRuntime;
use clap::{Args, Parser, ValueEnum};
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
        base_cli_utils::MetricsConfig::from(self.args.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::new().run_until_ctrl_c(self.args.exec())
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

    /// Optional L1 WebSocket endpoint for new-block subscriptions.
    ///
    /// When provided, the batcher subscribes to new L1 block headers over this
    /// WebSocket connection to advance the pipeline's L1 head. Without it,
    /// polling is used exclusively.
    #[arg(long = "l1-ws-url", env = "BATCHER_L1_WS_URL")]
    pub l1_ws_url: Option<Url>,

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
    pub private_key: PrivateKeySigner,

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

    /// Batch encoding mode.
    ///
    /// Accepts `single` / `0` and `span` / `1`. Span batches require Fjord
    /// to be active for the next L2 block at startup.
    #[arg(long = "batch-type", default_value = "single", env = "BATCHER_BATCH_TYPE")]
    batch_type: BatchTypeArg,
    /// Data availability mode for L1 submissions.
    ///
    /// Accepts `blobs` (default) or `calldata`.
    #[arg(
        long = "data-availability-type",
        default_value = "blobs",
        env = "BATCHER_DATA_AVAILABILITY_TYPE"
    )]
    da_type: base_batcher_encoder::DaType,

    /// Approximate compression ratio used for span batch size estimation.
    ///
    /// Only relevant when `--batch-type=span`. Should be slightly below the
    /// typical observed ratio to avoid creating a small leftover frame.
    /// Matches op-batcher's `--approx-compr-ratio` default.
    #[arg(long = "approx-compr-ratio", default_value = "0.6", env = "BATCHER_APPROX_COMPR_RATIO")]
    pub approx_compr_ratio: f64,

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

    /// Number of recent L1 blocks to scan on startup for already-submitted batcher frames.
    ///
    /// When set to a nonzero value N, the batcher walks back N L1 blocks from the
    /// current head on startup, decodes any calldata batcher frames it finds, and
    /// advances the L2 block cursor past data already pending on L1. This avoids
    /// re-submitting frames after an unclean shutdown. Maximum value is 128.
    ///
    /// A value of 0 (default) disables the scan. Matches op-batcher's
    /// `--check-recent-txs-depth` flag.
    #[arg(
        long = "check-recent-txs-depth",
        default_value = "0",
        value_parser = clap::value_parser!(u64).range(0..=128),
        env = "BATCHER_CHECK_RECENT_TXS_DEPTH"
    )]
    pub check_recent_txs_depth: u64,

    /// Bind address for the admin JSON-RPC API (default: 127.0.0.1).
    ///
    /// Only takes effect when `--admin-port` is also set.
    #[arg(long = "admin-addr", env = "BATCHER_ADMIN_ADDR", default_value = "127.0.0.1")]
    pub admin_addr: IpAddr,

    /// Port for the admin JSON-RPC API.
    ///
    /// When set, exposes `admin_startBatcher`, `admin_stopBatcher`,
    /// `admin_flushBatcher`, `admin_getThrottleController`, and related methods.
    /// When absent (default), the admin API is disabled.
    #[arg(long = "admin-port", env = "BATCHER_ADMIN_PORT")]
    pub admin_port: Option<u16>,

    /// Start in a stopped state, deferring batch submission until `admin_startBatcher` is called.
    ///
    /// The batcher connects to all endpoints and is fully observable but will not
    /// submit any batches until activated via the admin API. Useful for staged
    /// rollouts, controlled restarts, and debugging.
    #[arg(long = "stopped", env = "BATCHER_STOPPED")]
    pub stopped: bool,

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
            batch_type: self.batch_type.into(),
            da_type: self.da_type,
            approx_compr_ratio: self.approx_compr_ratio,
        };
        encoder_config.validate()?;
        Ok(BatcherConfig {
            l1_rpc_url: self.l1_rpc_url,
            l1_ws_url: self.l1_ws_url,
            l2_rpc_url: self.l2_rpc_url,
            l2_ws_url: self.l2_ws_url,
            rollup_rpc_url: self.rollup_rpc_url,
            batcher_private_key: Some(self.private_key),
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
            check_recent_txs_depth: self.check_recent_txs_depth,
            admin_addr: self.admin_port.map(|port| SocketAddr::new(self.admin_addr, port)),
            stopped: self.stopped,
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

        let rt = TokioRuntime::new();
        let _signal_handle = RuntimeManager::install_signal_handler(rt.token().clone());

        let service = BatcherService::new(config);
        service.setup(rt).await?.run().await
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BatchTypeArg {
    #[value(alias = "0")]
    Single,
    #[value(alias = "1")]
    Span,
}

impl From<BatchTypeArg> for base_batcher_encoder::BatchType {
    fn from(value: BatchTypeArg) -> Self {
        match value {
            BatchTypeArg::Single => Self::Single,
            BatchTypeArg::Span => Self::Span,
        }
    }
}
#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn base_args() -> Vec<&'static str> {
        vec![
            "base-batcher",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--l2-rpc-url",
            "http://localhost:9545",
            "--rollup-rpc-url",
            "http://localhost:7545",
            "--private-key",
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ]
    }

    fn parse_cli(extra: &[&'static str]) -> Cli {
        let mut args = base_args();
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).expect("CLI should parse")
    }

    #[test]
    fn into_config_defaults_to_single_batches_and_blobs() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.batch_type, base_batcher_encoder::BatchType::Single);
    }

    #[test]
    fn into_config_accepts_numeric_span_alias() {
        let cli = parse_cli(&["--batch-type", "1"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.batch_type, base_batcher_encoder::BatchType::Span);
    }

    #[test]
    fn into_config_defaults_to_blob_da() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Blob);
    }

    #[test]
    fn into_config_accepts_calldata_da_mode() {
        let cli = parse_cli(&["--data-availability-type", "calldata"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Calldata);
    }

    #[test]
    fn cli_rejects_auto_da_mode_for_now() {
        let mut args = base_args();
        args.extend_from_slice(["--data-availability-type", "auto"].as_slice());

        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn stopped_defaults_to_false() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");

        assert!(!config.stopped);
    }

    #[test]
    fn stopped_flag_sets_stopped_in_config() {
        let cli = parse_cli(&["--stopped"]);
        let config = cli.args.into_config().expect("config should build");

        assert!(config.stopped);
    }
}
