//! Arguments and startup for the `base batcher` subcommand.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::{ThrottleConfig, ThrottleStrategy};
use base_batcher_service::{BatcherConfig, BatcherService, ShadowConfig};
use base_cli_utils::RuntimeManager;
use base_runtime::TokioRuntime;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use clap::Parser;
use url::Url;

base_tx_manager::define_signer_cli!("BASE_BATCHER");

/// CLI arguments for the batcher.
#[derive(Parser, Clone, Debug)]
pub struct BatcherArgs {
    /// L1 RPC endpoint.
    #[arg(long = "l1-rpc-url", visible_aliases = ["l1", "l1-eth-rpc"], env = "BASE_NODE_L1_ETH_RPC")]
    pub l1_rpc_url: Url,

    /// Sequencer HTTP endpoints, comma-separated: conductors with their RPC proxy enabled, or
    /// consensus nodes started with `--rpc.execution-forwarding-endpoint`.
    ///
    /// The batcher reads the unsafe blocks of the leader and, outside shadow mode, follows its
    /// derivation. Among several endpoints, the leader is the first whose `admin_sequencerActive`
    /// answers `true`. The DA limits are pushed to every endpoint unless `--no-throttle` is set.
    #[arg(
        long = "sequencer-urls",
        env = "BASE_BATCHER_SEQUENCER_URLS",
        required = true,
        value_delimiter = ','
    )]
    pub sequencer_urls: Vec<Url>,

    /// Optional L1 WebSocket endpoint for new-block subscriptions.
    ///
    /// When provided, the batcher subscribes to new L1 block headers over this
    /// WebSocket connection to advance the pipeline's L1 head. Without it,
    /// polling is used exclusively.
    #[arg(long = "l1-ws-url", env = "BASE_BATCHER_L1_WS_URL")]
    pub l1_ws_url: Option<Url>,

    /// Signer configuration.
    #[command(flatten)]
    pub signer: SignerCli,

    /// Run as a shadow batcher.
    #[arg(long = "shadow.enabled", env = "BASE_BATCHER_SHADOW_ENABLED")]
    pub shadow_enabled: bool,

    /// The shadow inbox, which must be the batch inbox of the parity validator's rollup config.
    ///
    /// Required with `--shadow.enabled`.
    #[arg(long = "shadow.inbox", env = "BASE_BATCHER_SHADOW_INBOX")]
    pub shadow_inbox: Option<Address>,

    /// Parity validator rollup node RPC endpoint, whose rollup config the batcher reads
    /// and whose derivation it follows.
    ///
    /// Required with `--shadow.enabled`.
    #[arg(long = "shadow.validator-rollup-rpc", env = "BASE_BATCHER_SHADOW_VALIDATOR_ROLLUP_RPC")]
    pub shadow_validator_rollup_rpc: Option<Url>,

    /// Parity validator L2 RPC endpoint, whose derived block hashes are compared
    /// with the leader sequencer's.
    ///
    /// Required with `--shadow.enabled`.
    #[arg(long = "shadow.validator-l2-rpc", env = "BASE_BATCHER_SHADOW_VALIDATOR_L2_RPC")]
    pub shadow_validator_l2_rpc: Option<Url>,

    /// Polling interval in seconds.
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
    /// Above it, `--throttle-strategy` sets how far the batcher lowers the DA limits it
    /// pushes to the `--sequencer-urls` endpoints.
    #[arg(
        long = "throttle-threshold",
        default_value = "1000000",
        env = "BASE_BATCHER_THROTTLE_THRESHOLD"
    )]
    pub throttle_threshold: u64,

    /// How the throttle intensity, from 0 (highest DA limits) to 1 (lowest), grows with
    /// the DA backlog above `--throttle-threshold`.
    ///
    /// `off` never throttles but, unlike `--no-throttle`, keeps pushing the highest DA
    /// limits to the `--sequencer-urls` endpoints, so `admin_setThrottleController` can
    /// turn throttling on without a restart.
    #[arg(
        long = "throttle-strategy",
        default_value = "quadratic",
        env = "BASE_BATCHER_THROTTLE_STRATEGY"
    )]
    pub throttle_strategy: ThrottleStrategy,

    /// Disable DA throttling.
    ///
    /// The batcher never pushes DA limits to the `--sequencer-urls` endpoints, however large
    /// its DA backlog grows. Required with `--shadow.enabled`.
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

    /// Block startup until the rollup node whose derivation the batcher follows has
    /// processed the selected L1 target.
    ///
    /// By default the target is the current L1 head. `--check-recent-txs-depth`
    /// may select an earlier target from the configured window.
    #[arg(long = "wait-node-sync", env = "BASE_BATCHER_WAIT_NODE_SYNC")]
    pub wait_node_sync: bool,

    /// Budget for retrying one-shot startup RPCs, and the maximum seconds to wait for the
    /// rollup node the batcher follows to report sync when `--wait-node-sync` is set.
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
        // Shadow mode takes all four of its flags, and a canonical batcher none of them.
        let shadow = match (
            self.shadow_enabled,
            self.shadow_inbox,
            self.shadow_validator_rollup_rpc,
            self.shadow_validator_l2_rpc,
        ) {
            (true, Some(inbox), Some(validator_rollup_rpc), Some(validator_l2_rpc)) => {
                Some(ShadowConfig { inbox, validator_rollup_rpc, validator_l2_rpc })
            }
            (false, None, None, None) => None,
            _ => eyre::bail!(
                "--shadow.enabled, --shadow.inbox, --shadow.validator-rollup-rpc and \
                 --shadow.validator-l2-rpc must be set together"
            ),
        };

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
            sequencer_urls: self.sequencer_urls,
            signer: Some(signer),
            metrics_enabled,
            shadow,
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
            throttle_strategy: self.throttle_strategy,
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

    const PRIVATE_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn base_args_without_signer() -> Vec<&'static str> {
        vec![
            "batcher",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--sequencer-urls",
            "http://localhost:7545",
        ]
    }

    fn base_args() -> Vec<&'static str> {
        let mut args = base_args_without_signer();
        args.extend_from_slice(&["--private-key", PRIVATE_KEY]);
        args
    }

    fn parse_cli(extra: &[&'static str]) -> BatcherArgs {
        let mut args = base_args();
        args.extend_from_slice(extra);
        BatcherArgs::try_parse_from(args).expect("CLI should parse")
    }

    /// A remote signer endpoint and address configure the signer in place of a private key.
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

    /// `--sequencer-urls` takes a comma-separated list and keeps its order.
    #[test]
    fn sequencer_urls_are_comma_separated() {
        let cli = BatcherArgs::try_parse_from([
            "batcher",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--sequencer-urls",
            "http://conductor-0:8547,http://conductor-1:8547",
            "--private-key",
            PRIVATE_KEY,
        ])
        .expect("CLI should parse");
        let config = cli.into_config(false).expect("config should build");

        let sequencer_urls: Vec<&str> = config.sequencer_urls.iter().map(Url::as_str).collect();
        assert_eq!(sequencer_urls, ["http://conductor-0:8547/", "http://conductor-1:8547/"]);
    }

    /// Any one, two or three of the four shadow flags are refused with the same message.
    #[test]
    fn into_config_requires_the_shadow_flags_together() {
        let shadow_flags: [&[&'static str]; 4] = [
            &["--shadow.enabled"],
            &["--shadow.inbox", "0x1111111111111111111111111111111111111111"],
            &["--shadow.validator-rollup-rpc", "http://validator:7545"],
            &["--shadow.validator-l2-rpc", "http://validator:9545"],
        ];

        // Each bit of `subset` keeps one flag. The range leaves out the subset that keeps none
        // and the one that keeps all four.
        for subset in 1..(1 << shadow_flags.len()) - 1 {
            let flags: Vec<&str> = shadow_flags
                .iter()
                .enumerate()
                .filter(|(index, _)| subset & (1 << index) != 0)
                .flat_map(|(_, flag)| flag.iter().copied())
                .collect();

            let error = parse_cli(&flags).into_config(false).unwrap_err();

            assert_eq!(
                error.to_string(),
                "--shadow.enabled, --shadow.inbox, --shadow.validator-rollup-rpc and \
                 --shadow.validator-l2-rpc must be set together",
                "{flags:?}"
            );
        }
    }

    /// The four shadow flags together build a `ShadowConfig` holding the given inbox and parity
    /// validator URLs.
    #[test]
    fn into_config_builds_the_shadow_config() {
        let cli = parse_cli(&[
            "--shadow.enabled",
            "--shadow.inbox",
            "0x1111111111111111111111111111111111111111",
            "--shadow.validator-rollup-rpc",
            "http://validator:7545",
            "--shadow.validator-l2-rpc",
            "http://validator:9545",
            "--no-throttle",
        ]);
        let shadow = cli.into_config(false).unwrap().shadow.expect("a shadow config");

        assert_eq!(shadow.inbox, Address::repeat_byte(0x11));
        assert_eq!(shadow.validator_rollup_rpc.as_str(), "http://validator:7545/");
        assert_eq!(shadow.validator_l2_rpc.as_str(), "http://validator:9545/");
    }

    /// Without flags the batcher runs blobs at full blob frames and Brotli quality 9, picks the
    /// quadratic throttle strategy, starts running and does not wait for the node to sync.
    #[test]
    fn into_config_applies_the_defaults() {
        let cli = parse_cli(&[]);
        let config = cli.into_config(false).expect("config should build");

        assert!(!config.stopped);
        assert!(!config.wait_node_sync);
        assert_eq!(config.throttle_strategy, ThrottleStrategy::Quadratic);

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Blob);
        assert_eq!(
            config.encoder_config.max_frame_size,
            base_batcher_encoder::EncoderConfig::MAX_BLOB_FRAME_SIZE
        );
        assert_eq!(config.encoder_config.brotli_level, base_batcher_encoder::BrotliLevel::Brotli9);
    }

    /// A Brotli quality above 11, the encoder's highest level, is refused at parse time.
    #[test]
    fn cli_rejects_brotli_quality_out_of_range() {
        let mut args = base_args();
        args.extend_from_slice(["--brotli-quality", "12"].as_slice());

        assert!(BatcherArgs::try_parse_from(args).is_err());
    }

    /// A calldata batcher's frame size is its calldata cap minus the derivation version byte.
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

    /// Every encoder, submission, throttle, startup and admin flag reaches the config, so no
    /// operator flag is silently ignored.
    #[test]
    fn into_config_applies_the_operator_flags() {
        let cli = parse_cli(&[
            "--data-availability-type",
            "calldata",
            "--compressed-size-target",
            "1000",
            "--max-blobs-per-tx",
            "3",
            "--brotli-quality",
            "5",
            "--publish-max-retries",
            "7",
            "--publish-retry-delay",
            "3s",
            "--max-channel-duration",
            "10",
            "--sub-safety-margin",
            "4",
            "--max-pending-transactions",
            "4",
            "--num-confirmations",
            "3",
            "--resubmission-timeout",
            "30",
            "--poll-interval",
            "2",
            "--throttle-threshold",
            "500000",
            "--throttle-strategy",
            "linear",
            "--check-recent-txs-depth",
            "16",
            "--wait-node-sync-timeout",
            "60",
            "--admin-addr",
            "0.0.0.0",
            "--admin-port",
            "7000",
            "--l1-ws-url",
            "ws://localhost:8546",
            "--stopped",
            "--wait-node-sync",
        ]);
        let config = cli.into_config(false).expect("config should build");

        assert_eq!(config.encoder_config.da_type, base_batcher_encoder::DaType::Calldata);
        assert_eq!(config.encoder_config.compressed_size_target, Some(1000));
        assert_eq!(config.encoder_config.max_blobs_per_tx, 3);
        assert_eq!(config.encoder_config.brotli_level, base_batcher_encoder::BrotliLevel::Brotli5);
        assert_eq!(config.tx_manager.publish_max_retries, 7);
        assert_eq!(config.tx_manager.publish_retry_delay, Duration::from_secs(3));
        assert_eq!(config.encoder_config.max_channel_duration, 10);
        assert_eq!(config.encoder_config.sub_safety_margin, 4);
        assert_eq!(config.max_pending_transactions, 4);
        assert_eq!(config.tx_manager.num_confirmations, 3);
        assert_eq!(config.tx_manager.resubmission_timeout, Duration::from_secs(30));
        assert_eq!(config.poll_interval, Duration::from_secs(2));
        assert_eq!(config.throttle.expect("the throttle is on").threshold_bytes, 500_000);
        assert_eq!(config.throttle_strategy, ThrottleStrategy::Linear);
        assert_eq!(config.check_recent_txs_depth, 16);
        assert_eq!(config.wait_node_sync_timeout, Duration::from_secs(60));
        assert_eq!(config.admin_addr, Some(SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 7000)));
        assert_eq!(config.l1_ws_url.expect("a WebSocket URL").as_str(), "ws://localhost:8546/");
        assert!(config.stopped);
        assert!(config.wait_node_sync);
    }

    /// Parsing fails without `--l1-rpc-url` or without `--sequencer-urls`.
    #[test]
    fn cli_requires_the_l1_rpc_and_sequencer_urls() {
        let args = base_args();
        for flag in ["--l1-rpc-url", "--sequencer-urls"] {
            let position = args.iter().position(|arg| *arg == flag).unwrap();
            let mut without = args.clone();
            without.drain(position..position + 2);

            let error = BatcherArgs::try_parse_from(without).unwrap_err();

            assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument, "{flag}");
        }
    }

    /// The DA throttle is on unless `--no-throttle` is set, even with `--throttle-strategy off`.
    #[test]
    fn only_no_throttle_disables_the_da_throttle() {
        assert!(parse_cli(&[]).into_config(false).unwrap().throttle.is_some());
        assert!(parse_cli(&["--no-throttle"]).into_config(false).unwrap().throttle.is_none());

        let off = parse_cli(&["--throttle-strategy", "off"]).into_config(false).unwrap();
        assert!(off.throttle.is_some());
        assert_eq!(off.throttle_strategy, ThrottleStrategy::Off);
    }

    /// Throttling forces blobs unless `--no-force-blobs-when-throttling` is set.
    #[test]
    fn no_force_blobs_when_throttling_turns_blob_forcing_off() {
        assert!(parse_cli(&[]).into_config(false).unwrap().force_blobs_when_throttling);
        let cli = parse_cli(&["--no-force-blobs-when-throttling"]);
        assert!(!cli.into_config(false).unwrap().force_blobs_when_throttling);
    }
}
