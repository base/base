//! CLI argument parsing for the Base Batcher binary.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::ThrottleConfig;
use base_batcher_service::{BatcherConfig, BatcherService};
use base_cli_utils::{LogConfig, RuntimeManager};
use base_runtime::TokioRuntime;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use clap::{Args, Parser};
use tracing::info;
use url::Url;

base_cli_utils::define_log_args!("BATCHER");
base_cli_utils::define_metrics_args!("BATCHER", 7300);
base_tx_manager::define_signer_cli!("BATCHER");

/// The Base Batcher CLI.
#[derive(Parser, Clone, Debug)]
#[command(
    author,
    version = env!("CARGO_PKG_VERSION"),
    about = "Base Batcher — submits L2 batch data to L1",
    long_about = None
)]
pub struct Cli {
    /// Batcher arguments.
    #[command(flatten)]
    pub args: BatcherArgs,
}

impl Cli {
    /// Run the batcher CLI.
    pub fn run(self) -> eyre::Result<()> {
        LogConfig::from(self.args.logging.clone()).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(self.args.metrics.clone()).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::new().run_until_ctrl_c(self.args.exec())
    }
}

/// CLI arguments for the batcher.
#[derive(Args, Clone, Debug)]
pub struct BatcherArgs {
    /// L1 RPC endpoint(s).
    ///
    /// Accepts a comma-separated list. The service connects to each in order at
    /// startup and uses the first that responds; later endpoints serve as
    /// startup-time fallbacks only (no per-call rotation).
    #[arg(long = "l1-rpc-url", env = "BATCHER_L1_RPC_URL", value_delimiter = ',', num_args = 1..)]
    pub l1_rpc_url: Vec<Url>,

    /// Additional symmetric L1 transaction publication backends.
    ///
    /// Each destination preserves nonce order independently. The selected
    /// `--l1-rpc-url` endpoint is included as an equal publication backend and
    /// remains the separate source for chain reads and confirmations.
    #[arg(
        long = "publish-rpc-url",
        env = "BATCHER_PUBLISH_RPC_URL",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub publish_rpc_urls: Vec<Url>,

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

    /// Parity validator L2 RPC endpoint for shadow mode.
    ///
    /// Required with `--dangerously-override-batch-inbox-address` and rejected
    /// without it. Its safe L2 head anchors shadow batcher recovery, and its
    /// derived block hashes are compared with the sequencer.
    #[arg(long = "parity-validator-l2-rpc-url", env = "BATCHER_PARITY_VALIDATOR_L2_RPC_URL")]
    pub parity_validator_l2_rpc_url: Option<Url>,

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

    /// Signer configuration.
    #[command(flatten)]
    pub signer: SignerCli,

    /// Enable explicit shadow-mode guardrails for dangerous overrides.
    ///
    /// This flag does nothing by itself. It must be set together with
    /// `--dangerously-override-batch-inbox-address` so canonical deployments
    /// cannot accidentally redirect DA submissions.
    #[arg(long = "shadow-mode", env = "BATCHER_SHADOW_MODE")]
    pub shadow_mode: bool,

    /// Dangerous shadow-mode batch inbox override.
    ///
    /// Requires `--shadow-mode`. Canonical deployments must not set this flag.
    #[arg(
        long = "dangerously-override-batch-inbox-address",
        env = "BATCHER_DANGEROUSLY_OVERRIDE_BATCH_INBOX_ADDRESS"
    )]
    pub dangerously_override_batch_inbox_address: Option<Address>,

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

    /// Data availability mode for L1 submissions.
    ///
    /// Accepts `blobs` (default) or `calldata`.
    #[arg(
        long = "data-availability-type",
        default_value = "blobs",
        env = "BATCHER_DATA_AVAILABILITY_TYPE"
    )]
    pub da_type: base_batcher_encoder::DaType,

    /// Maximum number of in-flight (unconfirmed) transactions.
    #[arg(
        long = "max-pending-transactions",
        default_value = "1",
        env = "BATCHER_MAX_PENDING_TRANSACTIONS"
    )]
    pub max_pending_transactions: usize,

    /// Number of L1 confirmations before a tx is considered finalized.
    #[arg(long = "num-confirmations", default_value = "1", env = "BATCHER_NUM_CONFIRMATIONS")]
    pub num_confirmations: u64,

    /// Timeout before resubmitting a transaction (seconds).
    #[arg(
        long = "resubmission-timeout",
        default_value = "48",
        env = "BATCHER_RESUBMISSION_TIMEOUT"
    )]
    pub resubmission_timeout_secs: u64,

    /// Maximum fast retries after the initial publication attempt.
    #[arg(long = "publish-max-retries", default_value = "10", env = "BATCHER_PUBLISH_MAX_RETRIES")]
    pub publish_max_retries: usize,

    /// Delay between fast publication attempts.
    #[arg(
        long = "publish-retry-delay",
        default_value = "1s",
        env = "BATCHER_PUBLISH_RETRY_DELAY",
        value_parser = humantime::parse_duration
    )]
    pub publish_retry_delay: Duration,

    /// DA backlog threshold in bytes at which throttling activates.
    ///
    /// When the estimated unsubmitted DA backlog exceeds this value, the batcher
    /// signals the sequencer to reduce block throughput.
    #[arg(
        long = "throttle-threshold",
        default_value = "1000000",
        env = "BATCHER_THROTTLE_THRESHOLD"
    )]
    pub throttle_threshold: u64,

    /// Disable DA throttling.
    ///
    /// Pass this flag to submit batches at full rate regardless of DA backlog.
    #[arg(long = "no-throttle", env = "BATCHER_NO_THROTTLE")]
    pub no_throttle: bool,

    /// Number of recent L1 blocks to inspect for a confirmed batcher transaction.
    ///
    /// With `--wait-node-sync`, recent nonce activity selects the L1 synchronization
    /// target within this window.
    /// It does not decode batches or change the L2 backfill cursor. A non-zero
    /// value requires `--wait-node-sync`.
    ///
    /// A value of 0 (default) disables the scan.
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
    /// by the mempool. No-op for blob DA. Omit to disable the cap.
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

    /// Block startup until the rollup node has processed the selected L1 target.
    ///
    /// By default the target is the current L1 head. `--check-recent-txs-depth`
    /// may select an earlier target from the configured window.
    #[arg(long = "wait-node-sync", env = "BATCHER_WAIT_NODE_SYNC")]
    pub wait_node_sync: bool,

    /// Budget for retrying one-shot startup RPCs, and the maximum seconds to
    /// wait for the rollup node to report sync when `--wait-node-sync` is set.
    /// On expiry the service exits with an error rather than hanging
    /// indefinitely. Default: 600 seconds (10 minutes).
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
    /// is configured because blobs amortise DA cost more efficiently under congestion.
    /// Pass this flag to keep the configured DA type regardless of throttle state.
    /// No-op for blob-configured batchers.
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
    pub fn into_config(self) -> eyre::Result<BatcherConfig> {
        if self.shadow_mode != self.dangerously_override_batch_inbox_address.is_some() {
            eyre::bail!(
                "--shadow-mode and --dangerously-override-batch-inbox-address must be set together"
            );
        }
        let signer = SignerConfig::try_from(self.signer)?;
        let frame_size = match self.da_type {
            base_batcher_encoder::DaType::Blob => self
                .target_frame_size
                .saturating_sub(base_batcher_encoder::EncoderConfig::BLOB_DERIVATION_PREFIX_SIZE),
            base_batcher_encoder::DaType::Calldata => self.target_frame_size,
        };
        let encoder_config = base_batcher_encoder::EncoderConfig {
            target_frame_size: frame_size,
            max_frame_size: frame_size,
            max_channel_duration: self.max_channel_duration,
            sub_safety_margin: self.sub_safety_margin,
            target_num_frames: self.target_num_frames,
            da_type: self.da_type,
            // The batcher binary only targets post-Fjord chains, so it always uses Brotli.
            compression_algo: base_batcher_encoder::CompressionAlgo::Brotli10,
            max_l1_tx_size_bytes: self.max_l1_tx_size_bytes,
        };
        encoder_config.validate()?;
        let tx_manager = TxManagerConfig {
            num_confirmations: self.num_confirmations,
            resubmission_timeout: Duration::from_secs(self.resubmission_timeout_secs),
            publish_max_retries: self.publish_max_retries,
            publish_retry_delay: self.publish_retry_delay,
            ..TxManagerConfig::default()
        };
        tx_manager.validate()?;
        Ok(BatcherConfig {
            l1_rpc_url: self.l1_rpc_url,
            publish_rpc_urls: self.publish_rpc_urls,
            l1_ws_url: self.l1_ws_url,
            l2_rpc_url: self.l2_rpc_url,
            parity_validator_l2_rpc_url: self.parity_validator_l2_rpc_url,
            rollup_rpc_url: self.rollup_rpc_url,
            signer: Some(signer),
            metrics_enabled: self.metrics.enabled,
            batch_inbox_override: self.dangerously_override_batch_inbox_address,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            encoder_config,
            max_pending_transactions: self.max_pending_transactions,
            tx_manager,
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
    pub async fn exec(self) -> eyre::Result<()> {
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn base_args_without_signer() -> Vec<&'static str> {
        vec![
            "base-batcher",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--l2-rpc-url",
            "http://localhost:9545",
            "--rollup-rpc-url",
            "http://localhost:7545",
        ]
    }

    fn base_args() -> Vec<&'static str> {
        let mut args = base_args_without_signer();
        args.extend_from_slice(&[
            "--private-key",
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ]);
        args
    }

    fn parse_cli(extra: &[&'static str]) -> Cli {
        let mut args = base_args();
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).expect("CLI should parse")
    }

    #[test]
    fn into_config_accepts_remote_signer() {
        let mut args = base_args_without_signer();
        args.extend_from_slice(&[
            "--signer-endpoint",
            "http://127.0.0.1:9000",
            "--signer-address",
            "0x4242424242424242424242424242424242424242",
        ]);
        let cli = Cli::try_parse_from(args).expect("CLI should parse");
        let config = cli.args.into_config().expect("config should build");

        let signer = config.signer.expect("signer should be configured");
        assert_eq!(signer.address(), Address::repeat_byte(0x42));
    }

    #[test]
    fn into_config_sets_metrics_enabled() {
        let cli = parse_cli(&["--metrics.enabled"]);
        let config = cli.args.into_config().expect("config should build");

        assert!(config.metrics_enabled);
    }

    #[test]
    fn into_config_rejects_shadow_mode_without_batch_inbox_override() {
        let cli = parse_cli(&["--shadow-mode"]);
        let err = cli.args.into_config().expect_err("shadow mode alone should fail");

        assert!(
            err.to_string()
                .contains("--shadow-mode and --dangerously-override-batch-inbox-address")
        );
    }

    #[test]
    fn into_config_rejects_batch_inbox_override_without_shadow_mode() {
        let cli = parse_cli(&[
            "--dangerously-override-batch-inbox-address",
            "0x1111111111111111111111111111111111111111",
        ]);
        let err = cli.args.into_config().expect_err("override without shadow mode should fail");

        assert!(
            err.to_string()
                .contains("--shadow-mode and --dangerously-override-batch-inbox-address")
        );
    }

    #[test]
    fn into_config_accepts_shadow_batch_inbox_override() {
        let cli = parse_cli(&[
            "--shadow-mode",
            "--dangerously-override-batch-inbox-address",
            "0x1111111111111111111111111111111111111111",
        ]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.batch_inbox_override, Some(Address::repeat_byte(0x11)));
    }

    #[test]
    fn into_config_defaults_to_blob_da() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Blob);
    }

    #[test]
    fn into_config_reserves_blob_derivation_prefix_from_target_frame_size() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(
            config.encoder_config.target_frame_size,
            base_batcher_encoder::EncoderConfig::MAX_BLOB_FRAME_SIZE
        );
        assert_eq!(config.encoder_config.max_frame_size, config.encoder_config.target_frame_size);
    }

    #[test]
    fn into_config_reserves_blob_prefix_for_explicit_target_frame_size() {
        let cli = parse_cli(&["--target-frame-size", "130000"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.target_frame_size, 129_999);
        assert_eq!(config.encoder_config.max_frame_size, 129_999);
    }

    #[test]
    fn into_config_accepts_calldata_da_mode() {
        let cli = parse_cli(&["--data-availability-type", "calldata"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Calldata);
    }

    #[test]
    fn into_config_does_not_reserve_blob_prefix_for_calldata_da_mode() {
        let cli =
            parse_cli(&["--data-availability-type", "calldata", "--target-frame-size", "130000"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.encoder_config.target_frame_size, 130_000);
        assert_eq!(config.encoder_config.max_frame_size, 130_000);
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
    fn into_config_sets_publish_retry_policy() {
        let cli = parse_cli(&["--publish-max-retries", "12", "--publish-retry-delay", "250ms"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.tx_manager.publish_max_retries, 12);
        assert_eq!(config.tx_manager.publish_retry_delay, Duration::from_millis(250));
    }

    #[test]
    fn rpc_urls_default_to_single_endpoint() {
        let cli = parse_cli(&[]);
        let config = cli.args.into_config().expect("config should build");
        assert_eq!(config.l1_rpc_url.len(), 1);
        assert!(config.publish_rpc_urls.is_empty());
        assert_eq!(config.l2_rpc_url.len(), 1);
        assert_eq!(config.rollup_rpc_url.len(), 1);
    }

    #[test]
    fn publish_rpc_urls_accept_multiple_destinations() {
        let cli =
            parse_cli(&["--publish-rpc-url", "http://backend-a.example,http://backend-b.example"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.publish_rpc_urls.len(), 2);
        assert_eq!(config.publish_rpc_urls[0].as_str(), "http://backend-a.example/");
        assert_eq!(config.publish_rpc_urls[1].as_str(), "http://backend-b.example/");
    }

    #[test]
    fn into_config_accepts_parity_validator_l2_rpc_url() {
        let cli = parse_cli(&["--parity-validator-l2-rpc-url", "http://127.0.0.1:9545"]);
        let config = cli.args.into_config().expect("config should build");

        assert_eq!(config.parity_validator_l2_rpc_url.unwrap().as_str(), "http://127.0.0.1:9545/");
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
