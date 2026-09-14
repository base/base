//! Arguments and startup for the `base batcher` subcommand.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::ThrottleConfig;
use base_batcher_service::{BatcherConfig, BatcherService};
use base_cli_utils::RuntimeManager;
use base_runtime::TokioRuntime;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use clap::Parser;
use tracing::info;
use url::Url;

base_tx_manager::define_signer_cli!("BASE_BATCHER");

/// CLI arguments for the batcher.
#[derive(Parser, Clone, Debug)]
pub struct BatcherArgs {
    /// L1 RPC endpoint(s).
    ///
    /// Accepts a comma-separated list. The service connects to each in order at
    /// startup and uses the first that responds; later endpoints serve as
    /// startup-time fallbacks only (no per-call rotation).
    #[arg(long = "l1-rpc-url", visible_aliases = ["l1", "l1-eth-rpc"], env = "BASE_NODE_L1_ETH_RPC", value_delimiter = ',', num_args = 1..)]
    pub l1_rpc_url: Vec<Url>,

    /// L2 HTTP RPC endpoint(s) (used for all JSON-RPC calls including throttle control).
    ///
    /// Accepts a comma-separated list with the same connection-time failover
    /// semantics as `--l1-rpc-url`.
    #[arg(long = "l2-rpc-url", env = "BASE_BATCHER_L2_RPC_URL", value_delimiter = ',', num_args = 1..)]
    pub l2_rpc_url: Vec<Url>,

    /// Optional L1 WebSocket endpoint for new-block subscriptions.
    ///
    /// When provided, the batcher subscribes to new L1 block headers over this
    /// WebSocket connection to advance the pipeline's L1 head. Without it,
    /// polling is used exclusively.
    #[arg(long = "l1-ws-url", env = "BASE_BATCHER_L1_WS_URL")]
    pub l1_ws_url: Option<Url>,

    /// Parity validator L2 RPC endpoint for shadow mode.
    ///
    /// Required with `--dangerously-override-batch-inbox-address` and rejected
    /// without it. Its safe L2 head anchors shadow batcher recovery, and its
    /// derived block hashes are compared with the sequencer.
    #[arg(long = "parity-validator-l2-rpc-url", env = "BASE_BATCHER_PARITY_VALIDATOR_L2_RPC_URL")]
    pub parity_validator_l2_rpc_url: Option<Url>,

    /// Rollup node RPC endpoint(s).
    ///
    /// Accepts a comma-separated list with the same connection-time failover
    /// semantics as `--l1-rpc-url`.
    #[arg(
        long = "rollup-rpc-url",
        env = "BASE_BATCHER_ROLLUP_RPC_URL",
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
    #[arg(long = "shadow-mode", env = "BASE_BATCHER_SHADOW_MODE")]
    pub shadow_mode: bool,

    /// Dangerous shadow-mode batch inbox override.
    ///
    /// Requires `--shadow-mode`. Canonical deployments must not set this flag.
    #[arg(
        long = "dangerously-override-batch-inbox-address",
        env = "BASE_BATCHER_DANGEROUSLY_OVERRIDE_BATCH_INBOX_ADDRESS"
    )]
    pub dangerously_override_batch_inbox_address: Option<Address>,

    /// L2 block polling interval in seconds.
    #[arg(long = "poll-interval", default_value = "1", env = "BASE_BATCHER_POLL_INTERVAL")]
    pub poll_interval_secs: u64,

    /// Maximum L1 blocks a channel may stay open.
    #[arg(
        long = "max-channel-duration",
        default_value = "2",
        env = "BASE_BATCHER_MAX_CHANNEL_DURATION"
    )]
    pub max_channel_duration: u64,

    /// L1 blocks subtracted from `max-channel-duration` for the close deadline.
    ///
    /// Must be smaller than `max-channel-duration`.
    #[arg(long = "sub-safety-margin", default_value = "0", env = "BASE_BATCHER_SUB_SAFETY_MARGIN")]
    pub sub_safety_margin: u64,

    /// Optional soft target for stable bytes emitted by the compression stream.
    ///
    /// The reaching batch stays in the channel, then the channel closes.
    #[arg(long = "compressed-size-target", env = "BASE_BATCHER_COMPRESSED_SIZE_TARGET")]
    pub compressed_size_target: Option<usize>,

    /// Maximum number of blobs per L1 transaction (hard maximum: 6).
    #[arg(long = "max-blobs-per-tx", default_value = "6", env = "BASE_BATCHER_MAX_BLOBS_PER_TX")]
    pub max_blobs_per_tx: usize,

    /// Brotli quality (`0..=11`).
    #[arg(
        long = "brotli-quality",
        default_value_t = base_batcher_encoder::BrotliLevel::DEFAULT.as_u32() as u8,
        env = "BASE_BATCHER_BROTLI_QUALITY",
        value_parser = clap::value_parser!(u8).range(0..=11)
    )]
    pub brotli_quality: u8,

    /// Data availability mode for L1 submissions.
    ///
    /// Accepts `blobs` (default) or `calldata`.
    #[arg(
        long = "data-availability-type",
        default_value = "blobs",
        env = "BASE_BATCHER_DATA_AVAILABILITY_TYPE"
    )]
    pub da_type: base_batcher_encoder::DaType,

    /// Maximum number of in-flight (unconfirmed) transactions.
    #[arg(
        long = "max-pending-transactions",
        default_value = "1",
        env = "BASE_BATCHER_MAX_PENDING_TRANSACTIONS"
    )]
    pub max_pending_transactions: usize,

    /// Number of L1 confirmations before a tx is considered finalized.
    #[arg(long = "num-confirmations", default_value = "1", env = "BASE_BATCHER_NUM_CONFIRMATIONS")]
    pub num_confirmations: u64,

    /// Timeout before resubmitting a transaction (seconds).
    #[arg(
        long = "resubmission-timeout",
        default_value = "48",
        env = "BASE_BATCHER_RESUBMISSION_TIMEOUT"
    )]
    pub resubmission_timeout_secs: u64,

    /// Maximum retries when an RPC temporarily rejects an ordered nonce.
    #[arg(
        long = "publish-max-retries",
        default_value = "10",
        env = "BASE_BATCHER_PUBLISH_MAX_RETRIES"
    )]
    pub publish_max_retries: usize,

    /// Delay between nonce-too-high publication retries.
    #[arg(
        long = "publish-retry-delay",
        default_value = "1s",
        env = "BASE_BATCHER_PUBLISH_RETRY_DELAY",
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
        env = "BASE_BATCHER_THROTTLE_THRESHOLD"
    )]
    pub throttle_threshold: u64,

    /// Disable DA throttling.
    ///
    /// Pass this flag to submit batches at full rate regardless of DA backlog.
    #[arg(long = "no-throttle", env = "BASE_BATCHER_NO_THROTTLE")]
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
        env = "BASE_BATCHER_CHECK_RECENT_TXS_DEPTH"
    )]
    pub check_recent_txs_depth: u64,

    /// Maximum derivation payload carried in one calldata transaction.
    ///
    /// Includes the derivation-version prefix but excludes the signed transaction
    /// envelope. No-op for blob DA. Omit to use the blob-compatible frame limit.
    #[arg(long = "max-calldata-size-bytes", env = "BASE_BATCHER_MAX_CALLDATA_SIZE_BYTES")]
    pub max_calldata_size_bytes: Option<usize>,

    /// Bind address for the admin JSON-RPC API (default: 127.0.0.1).
    ///
    /// Only takes effect when `--admin-port` is also set.
    #[arg(long = "admin-addr", env = "BASE_BATCHER_ADMIN_ADDR", default_value = "127.0.0.1")]
    pub admin_addr: IpAddr,

    /// Port for the admin JSON-RPC API.
    ///
    /// When set, exposes `admin_startBatcher`, `admin_stopBatcher`,
    /// `admin_flushBatcher`, `admin_getThrottleController`, and related methods.
    /// When absent (default), the admin API is disabled.
    #[arg(long = "admin-port", env = "BASE_BATCHER_ADMIN_PORT")]
    pub admin_port: Option<u16>,

    /// Start in a stopped state, deferring batch submission until `admin_startBatcher` is called.
    ///
    /// The batcher connects to all endpoints and is fully observable but will not
    /// submit any batches until activated via the admin API. Useful for staged
    /// rollouts, controlled restarts, and debugging.
    #[arg(long = "stopped", env = "BASE_BATCHER_STOPPED")]
    pub stopped: bool,

    /// Block startup until the rollup node has processed the selected L1 target.
    ///
    /// By default the target is the current L1 head. `--check-recent-txs-depth`
    /// may select an earlier target from the configured window.
    #[arg(long = "wait-node-sync", env = "BASE_BATCHER_WAIT_NODE_SYNC")]
    pub wait_node_sync: bool,

    /// Budget for retrying one-shot startup RPCs, and the maximum seconds to
    /// wait for the rollup node to report sync when `--wait-node-sync` is set.
    /// On expiry the service exits with an error rather than hanging
    /// indefinitely. Default: 600 seconds (10 minutes).
    #[arg(
        long = "wait-node-sync-timeout",
        default_value = "600",
        env = "BASE_BATCHER_WAIT_NODE_SYNC_TIMEOUT"
    )]
    pub wait_node_sync_timeout_secs: u64,

    /// Keep the configured DA type when throttling.
    ///
    /// By default, throttling forces blob submissions even for calldata-configured
    /// batchers. This flag is a no-op when blob DA is already configured.
    #[arg(
        long = "no-force-blobs-when-throttling",
        env = "BASE_BATCHER_NO_FORCE_BLOBS_WHEN_THROTTLING"
    )]
    pub no_force_blobs_when_throttling: bool,
}

impl BatcherArgs {
    /// Convert CLI arguments into a [`BatcherConfig`].
    pub fn into_config(self, metrics_enabled: bool) -> eyre::Result<BatcherConfig> {
        // Shadow mode must never run against the configured production inbox.
        if self.shadow_mode != self.dangerously_override_batch_inbox_address.is_some() {
            eyre::bail!(
                "--shadow-mode and --dangerously-override-batch-inbox-address must be set together"
            );
        }

        let signer = SignerConfig::try_from(self.signer)?;

        // Blob frames use the full protocol packing limit. Calldata reserves
        // one byte for the derivation version outside the encoded frame.
        let max_frame_size = match self.da_type {
            base_batcher_encoder::DaType::Blob => {
                base_batcher_encoder::EncoderConfig::MAX_BLOB_FRAME_SIZE
            }
            base_batcher_encoder::DaType::Calldata => self
                .max_calldata_size_bytes
                .map_or(base_batcher_encoder::EncoderConfig::MAX_BLOB_FRAME_SIZE, |size| {
                    size.saturating_sub(1)
                }),
        };

        let brotli_level = base_batcher_encoder::BrotliLevel::from_u8(self.brotli_quality)
            .expect("clap restricts Brotli quality to 0..=11");
        let encoder_config = base_batcher_encoder::EncoderConfig {
            compressed_size_target: self.compressed_size_target,
            max_frame_size,
            max_channel_duration: self.max_channel_duration,
            sub_safety_margin: self.sub_safety_margin,
            max_blobs_per_tx: self.max_blobs_per_tx,
            da_type: self.da_type,
            brotli_level,
        };

        // Fail at startup, before constructing the service or accepting blocks.
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
            l1_ws_url: self.l1_ws_url,
            l2_rpc_url: self.l2_rpc_url,
            parity_validator_l2_rpc_url: self.parity_validator_l2_rpc_url,
            rollup_rpc_url: self.rollup_rpc_url,
            signer: Some(signer),
            metrics_enabled,
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
    pub async fn exec(self, metrics_enabled: bool) -> eyre::Result<()> {
        let config = self.into_config(metrics_enabled)?;
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
            "batcher",
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

    fn parse_cli(extra: &[&'static str]) -> BatcherArgs {
        let mut args = base_args();
        args.extend_from_slice(extra);
        BatcherArgs::try_parse_from(args).expect("CLI should parse")
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
        let cli = BatcherArgs::try_parse_from(args).expect("CLI should parse");
        let config = cli.into_config(false).expect("config should build");

        let signer = config.signer.expect("signer should be configured");
        assert_eq!(signer.address(), Address::repeat_byte(0x42));
    }

    #[test]
    fn into_config_sets_metrics_enabled() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(true).expect("config should build");

        assert!(config.metrics_enabled);
    }

    #[test]
    fn into_config_rejects_shadow_mode_without_batch_inbox_override() {
        let cli = parse_cli(&["--shadow-mode"]);
        let err = cli.into_config(false).expect_err("shadow mode alone should fail");

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
        let err = cli.into_config(false).expect_err("override without shadow mode should fail");

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
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.batch_inbox_override, Some(Address::repeat_byte(0x11)));
    }

    #[test]
    fn into_config_defaults_to_blob_da() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Blob);
    }

    #[test]
    fn into_config_uses_full_blob_frame_capacity() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(
            config.encoder_config.max_frame_size,
            base_batcher_encoder::EncoderConfig::MAX_BLOB_FRAME_SIZE
        );
        assert_eq!(config.encoder_config.compressed_size_target, None);
        assert_eq!(config.encoder_config.max_blobs_per_tx, 6);
        assert_eq!(config.encoder_config.brotli_level, base_batcher_encoder::BrotliLevel::Brotli10);
    }

    #[test]
    fn into_config_accepts_compressed_target_and_blob_limit() {
        let cli = parse_cli(&["--compressed-size-target", "700000", "--max-blobs-per-tx", "4"]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.encoder_config.compressed_size_target, Some(700_000));
        assert_eq!(config.encoder_config.max_blobs_per_tx, 4);
    }

    #[test]
    fn into_config_accepts_brotli_quality() {
        let cli = parse_cli(&["--brotli-quality", "9"]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.encoder_config.brotli_level, base_batcher_encoder::BrotliLevel::Brotli9);
    }

    #[test]
    fn cli_rejects_brotli_quality_out_of_range() {
        let mut args = base_args();
        args.extend_from_slice(["--brotli-quality", "12"].as_slice());

        assert!(BatcherArgs::try_parse_from(args).is_err());
    }

    #[test]
    fn into_config_accepts_calldata_da_mode() {
        let cli = parse_cli(&["--data-availability-type", "calldata"]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Calldata);
    }

    #[test]
    fn into_config_reserves_derivation_prefix_from_calldata_size_cap() {
        let cli = parse_cli(&[
            "--data-availability-type",
            "calldata",
            "--max-calldata-size-bytes",
            "130000",
        ]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.encoder_config.max_frame_size, 129_999);
    }

    #[test]
    fn cli_rejects_auto_da_mode_for_now() {
        let mut args = base_args();
        args.extend_from_slice(["--data-availability-type", "auto"].as_slice());

        assert!(BatcherArgs::try_parse_from(args).is_err());
    }

    #[test]
    fn stopped_defaults_to_false() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");

        assert!(!config.stopped);
    }

    #[test]
    fn stopped_flag_sets_stopped_in_config() {
        let cli = parse_cli(&["--stopped"]);
        let config = cli.into_config(false).expect("config should build");

        assert!(config.stopped);
    }

    #[test]
    fn into_config_sets_publish_retry_policy() {
        let cli = parse_cli(&["--publish-max-retries", "12", "--publish-retry-delay", "250ms"]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.tx_manager.publish_max_retries, 12);
        assert_eq!(config.tx_manager.publish_retry_delay, Duration::from_millis(250));
    }

    #[test]
    fn rpc_urls_default_to_single_endpoint() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");
        assert_eq!(config.l1_rpc_url.len(), 1);
        assert_eq!(config.l2_rpc_url.len(), 1);
        assert_eq!(config.rollup_rpc_url.len(), 1);
    }

    #[test]
    fn into_config_accepts_parity_validator_l2_rpc_url() {
        let cli = parse_cli(&["--parity-validator-l2-rpc-url", "http://127.0.0.1:9545"]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.parity_validator_l2_rpc_url.unwrap().as_str(), "http://127.0.0.1:9545/");
    }

    #[test]
    fn rpc_urls_accept_comma_separated_list() {
        // base_args() already sets `--l1-rpc-url http://localhost:8545`, so
        // appending a second `--l1-rpc-url` with three comma-separated values
        // accumulates: clap appends rather than overrides for `Vec` args.
        let cli =
            parse_cli(&["--l1-rpc-url", "http://l1-a:8545,http://l1-b:8545,http://l1-c:8545"]);
        let config = cli.into_config(false).expect("config should build");
        assert_eq!(config.l1_rpc_url.len(), 4);
        assert_eq!(config.l1_rpc_url[0].as_str(), "http://localhost:8545/");
        assert_eq!(config.l1_rpc_url[1].as_str(), "http://l1-a:8545/");
        assert_eq!(config.l1_rpc_url[3].as_str(), "http://l1-c:8545/");
    }

    #[test]
    fn wait_node_sync_defaults_to_false() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");
        assert!(!config.wait_node_sync);
    }

    #[test]
    fn wait_node_sync_flag_sets_config() {
        let cli = parse_cli(&["--wait-node-sync"]);
        let config = cli.into_config(false).expect("config should build");
        assert!(config.wait_node_sync);
    }

    #[test]
    fn force_blobs_when_throttling_defaults_to_true() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");
        assert!(config.force_blobs_when_throttling);
    }

    #[test]
    fn no_force_blobs_when_throttling_flag_inverts_default() {
        let cli = parse_cli(&["--no-force-blobs-when-throttling"]);
        let config = cli.into_config(false).expect("config should build");
        assert!(!config.force_blobs_when_throttling);
    }
}
