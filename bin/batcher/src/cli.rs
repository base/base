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
    /// L1 RPC endpoint(s).
    ///
    /// Accepts a comma-separated list. The service connects to each in order at
    /// startup and uses the first that responds; later endpoints serve as
    /// startup-time fallbacks only (no per-call rotation).
    #[arg(long = "l1-rpc-url", env = "BATCHER_L1_RPC_URL", value_delimiter = ',', num_args = 1..)]
    pub l1_rpc_url: Vec<Url>,

    /// L2 HTTP RPC endpoint(s) (used for all JSON-RPC calls including throttle control).
    ///
    /// Accepts a comma-separated list with the same connection-time failover
    /// semantics as `--l1-rpc-url`.
    #[arg(long = "l2-rpc-url", env = "BATCHER_L2_RPC_URL", value_delimiter = ',', num_args = 1..)]
    pub l2_rpc_url: Vec<Url>,

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

    /// Rollup node RPC endpoint(s).
    ///
    /// Accepts a comma-separated list with the same connection-time failover
    /// semantics as `--l1-rpc-url`.
    #[arg(
        long = "rollup-rpc-url",
        env = "BATCHER_ROLLUP_RPC_URL",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub rollup_rpc_url: Vec<Url>,

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

    /// Maximum serialized size of a single L1 calldata transaction in bytes.
    ///
    /// Safety cap that prevents oversized calldata transactions from being rejected
    /// by the mempool. No-op for blob DA. Equivalent to op-batcher's
    /// `--max-l1-tx-size-bytes` (default 120,000 bytes). Omit to disable the cap.
    #[arg(long = "max-l1-tx-size-bytes", env = "BATCHER_MAX_L1_TX_SIZE_BYTES")]
    pub max_l1_tx_size_bytes: Option<usize>,

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

    /// Block startup until the rollup node reports a non-zero sync status.
    ///
    /// Polls `optimism_syncStatus` on the poll interval until both `current_l1`
    /// and `unsafe_l2` heads are non-zero. Useful when the batcher is started
    /// alongside a fresh node so it does not race the node's initial sync.
    /// Matches op-batcher's `--wait-node-sync`.
    #[arg(long = "wait-node-sync", env = "BATCHER_WAIT_NODE_SYNC")]
    pub wait_node_sync: bool,

    /// Maximum seconds to wait for the rollup node to report sync when
    /// `--wait-node-sync` is set. On expiry the service exits with an error
    /// rather than hanging indefinitely. Default: 600 seconds (10 minutes).
    #[arg(
        long = "wait-node-sync-timeout",
        default_value = "600",
        env = "BATCHER_WAIT_NODE_SYNC_TIMEOUT"
    )]
    pub wait_node_sync_timeout_secs: u64,

    /// Disable the throttle-driven blob-DA override.
    ///
    /// By default, when DA-backlog throttling activates, the encoder is forced
    /// to emit blob-typed submissions even if `--data-availability-type=calldata`
    /// is configured (matching op-batcher's behaviour, since blobs amortise DA
    /// cost more efficiently under congestion). Pass this flag to keep the
    /// configured DA type regardless of throttle state. No-op for blob-configured
    /// batchers.
    #[arg(long = "no-force-blobs-when-throttling", env = "BATCHER_NO_FORCE_BLOBS_WHEN_THROTTLING")]
    pub no_force_blobs_when_throttling: bool,

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
            max_l1_tx_size_bytes: self.max_l1_tx_size_bytes,
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
            wait_node_sync: self.wait_node_sync,
            wait_node_sync_timeout: Duration::from_secs(self.wait_node_sync_timeout_secs),
            force_blobs_when_throttling: !self.no_force_blobs_when_throttling,
        })
    }

    /// Execute the batcher.
    async fn exec(self) -> eyre::Result<()> {
        let config = self.into_config()?;
        info!(
            l1_rpc_count = config.l1_rpc_url.len(),
            l2_rpc_count = config.l2_rpc_url.len(),
            rollup_rpc_count = config.rollup_rpc_url.len(),
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

impl From<BatchTypeArg> for base_protocol::BatchType {
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

        assert_eq!(config.encoder_config.batch_type, base_protocol::BatchType::Single);
    }

    #[test]
    fn into_config_accepts_numeric_span_alias() {
        let cli = parse_cli(&["--batch-type", "1"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.batch_type, base_protocol::BatchType::Span);
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

    #[test]
    fn rpc_urls_default_to_single_endpoint() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");
        assert_eq!(config.l1_rpc_url.len(), 1);
        assert_eq!(config.l2_rpc_url.len(), 1);
        assert_eq!(config.rollup_rpc_url.len(), 1);
    }

    #[test]
    fn rpc_urls_accept_comma_separated_list() {
        // base_args() already sets `--l1-rpc-url http://localhost:8545`, so
        // appending a second `--l1-rpc-url` with three comma-separated values
        // accumulates: clap appends rather than overrides for `Vec` args.
        let cli =
            parse_cli(&["--l1-rpc-url", "http://l1-a:8545,http://l1-b:8545,http://l1-c:8545"]);
        let config = cli.args.into_config().expect("config should build");
        assert_eq!(config.l1_rpc_url.len(), 4);
        assert_eq!(config.l1_rpc_url[0].as_str(), "http://localhost:8545/");
        assert_eq!(config.l1_rpc_url[1].as_str(), "http://l1-a:8545/");
        assert_eq!(config.l1_rpc_url[3].as_str(), "http://l1-c:8545/");
    }

    #[test]
    fn wait_node_sync_defaults_to_false() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");
        assert!(!config.wait_node_sync);
    }

    #[test]
    fn wait_node_sync_flag_sets_config() {
        let cli = parse_cli(&["--wait-node-sync"]);
        let config = cli.args.into_config().expect("config should build");
        assert!(config.wait_node_sync);
    }

    #[test]
    fn force_blobs_when_throttling_defaults_to_true() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");
        assert!(config.force_blobs_when_throttling);
    }

    #[test]
    fn no_force_blobs_when_throttling_flag_inverts_default() {
        let cli = parse_cli(&["--no-force-blobs-when-throttling"]);
        let config = cli.args.into_config().expect("config should build");
        assert!(!config.force_blobs_when_throttling);
    }
}
