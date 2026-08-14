//! Configuration types and validation for the challenger.

use std::{net::SocketAddr, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::MetricsConfig;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use eyre::{Result, WrapErr, ensure};
use url::Url;

use crate::cli::Cli;

/// Challenger configuration.
#[derive(Debug)]
pub struct ChallengerConfig {
    /// URL of the L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Url,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Url,
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory_addr: Address,
    /// Address of the `AnchorStateRegistry` contract on L1.
    pub anchor_state_registry_addr: Address,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// Polling interval for new dispute games.
    pub poll_interval: Duration,
    /// Run in no-dispute mode: skip all ZK/proof-dispute paths and run only the
    /// bond/anchor lifecycle.
    pub no_dispute: bool,
    /// URL of the ZK RPC endpoint. `None` in `--no-dispute` mode.
    pub zk_rpc_url: Option<Url>,
    /// Timeout for individual gRPC requests to the ZK proof service.
    pub zk_request_timeout: Duration,
    /// Maximum wall-clock time to wait for a ZK proof session before treating it as failed.
    pub max_proof_duration: Duration,
    /// Retryable TEE submission failures to tolerate before falling back to ZK.
    pub tee_submit_retry_limit: u32,
    /// Signing configuration for L1 transaction submission.
    pub signing: SignerConfig,
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    pub tx_manager: TxManagerConfig,
    /// Number of recent factory games scanned by bond discovery.
    pub bond_discovery_lookback_games: u64,
    /// How often a full rescan of the bond lookback window is performed.
    pub bond_discovery_interval: Duration,
    /// Addresses to claim bonds on behalf of.
    pub bond_claim_addresses: Vec<Address>,
    /// Maximum bond-lifecycle calls batched into a single `Multicall3` transaction. `1` disables
    /// batching and sends one transaction per call.
    pub bond_batch_size: usize,
    /// Address of the `Multicall3` deployment used when batching is enabled.
    pub multicall3_addr: Address,
    /// Health server socket address.
    pub health_addr: SocketAddr,
    /// Metrics server configuration.
    pub metrics: MetricsConfig,
}

impl ChallengerConfig {
    /// Creates a validated [`ChallengerConfig`] from parsed CLI arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if any validation check fails.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let Cli { challenger, metrics, health, .. } = cli;

        for (url, message) in [
            (&challenger.l1_eth_rpc, "invalid l1-eth-rpc URL: missing host"),
            (&challenger.l2_eth_rpc, "invalid l2-eth-rpc URL: missing host"),
        ] {
            ensure!(url.has_host(), message);
        }

        // Mode gating: `--no-dispute` strips the proving path, so it forbids
        // `--zk-rpc-url` and requires `--bond-claim-addresses` (otherwise the
        // driver would be a silent no-op). When disputing, `--zk-rpc-url` is
        // mandatory because the prover-service is the only proof backend.
        let no_dispute = challenger.no_dispute;
        if no_dispute {
            ensure!(
                challenger.zk_rpc_url.is_none(),
                "--zk-rpc-url must not be set in --no-dispute mode"
            );
            ensure!(
                !challenger.bond_claim_addresses.is_empty(),
                "--bond-claim-addresses is required in --no-dispute mode"
            );
        } else {
            ensure!(
                challenger.zk_rpc_url.is_some(),
                "--zk-rpc-url is required unless --no-dispute is set"
            );
        }
        if let Some(zk_rpc_url) = &challenger.zk_rpc_url {
            ensure!(zk_rpc_url.has_host(), "invalid zk-rpc-url URL: missing host");
        }

        ensure!(
            challenger.anchor_state_registry_addr != Address::ZERO,
            "anchor-state-registry-addr must be non-zero"
        );

        for (duration, message) in [
            (challenger.poll_interval, "poll-interval must be greater than 0"),
            (challenger.zk_request_timeout, "zk-request-timeout must be greater than 0"),
            (challenger.max_proof_duration, "max-proof-duration must be greater than 0"),
            (challenger.bond_discovery_interval, "bond-discovery-interval must be greater than 0"),
        ] {
            ensure!(!duration.is_zero(), message);
        }

        ensure!(
            challenger.bond_discovery_lookback_games != 0,
            "bond-discovery-lookback-games must be greater than 0"
        );

        ensure!(challenger.bond_batch_size != 0, "bond-batch-size must be greater than 0");
        ensure!(
            challenger.bond_batch_size == 1 || challenger.multicall3_addr != Address::ZERO,
            "multicall3-addr must be non-zero when bond-batch-size is greater than 1"
        );

        ensure!(health.port != 0, "health.port must be greater than 0");

        ensure!(
            !metrics.enabled || metrics.port != 0,
            "metrics.port must be greater than 0 when metrics are enabled"
        );

        Ok(Self {
            l1_eth_rpc: challenger.l1_eth_rpc,
            l2_eth_rpc: challenger.l2_eth_rpc,
            dispute_game_factory_addr: challenger.dispute_game_factory_addr,
            anchor_state_registry_addr: challenger.anchor_state_registry_addr,
            game_type: challenger.game_type,
            poll_interval: challenger.poll_interval,
            no_dispute,
            zk_rpc_url: challenger.zk_rpc_url,
            zk_request_timeout: challenger.zk_request_timeout,
            max_proof_duration: challenger.max_proof_duration,
            tee_submit_retry_limit: challenger.tee_submit_retry_limit,
            signing: SignerConfig::try_from(challenger.signer)
                .wrap_err("invalid signing config")?,
            tx_manager: TxManagerConfig::try_from(challenger.tx_manager)
                .wrap_err("invalid tx manager config")?,
            bond_discovery_lookback_games: challenger.bond_discovery_lookback_games,
            bond_discovery_interval: challenger.bond_discovery_interval,
            bond_claim_addresses: challenger.bond_claim_addresses,
            bond_batch_size: challenger.bond_batch_size,
            multicall3_addr: challenger.multicall3_addr,
            health_addr: health.socket_addr(),
            metrics: metrics.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use rstest::rstest;

    use super::*;

    /// Parse a mock CLI command with required args plus any overrides.
    ///
    /// The base defaults do **not** include signer flags (`--private-key` /
    /// `--signer-endpoint` / `--signer-address`). Tests that need a signer
    /// should pass those flags via `extra_args`.
    ///
    /// Keys present in `extra_args` replace their base defaults so clap never
    /// sees the same flag twice.
    fn cli_from_args(extra_args: &[&str]) -> Cli {
        let base: &[(&str, &str)] = &[
            ("--l1-eth-rpc", "http://localhost:8545"),
            ("--l2-eth-rpc", "http://localhost:9545"),
            ("--dispute-game-factory-addr", "0x1234567890123456789012345678901234567890"),
            ("--anchor-state-registry-addr", "0x2234567890123456789012345678901234567890"),
            ("--game-type", "1"),
            ("--zk-rpc-url", "http://localhost:5000"),
        ];

        let mut args = vec!["challenger"];
        for (key, value) in base {
            if !extra_args.contains(key) {
                args.push(key);
                args.push(value);
            }
        }
        args.extend_from_slice(extra_args);
        Cli::try_parse_from(args).unwrap()
    }

    /// Remote signer CLI flags for tests that need a valid signing configuration.
    const SIGNER_ARGS: [&str; 4] = [
        "--signer-endpoint",
        "http://localhost:8546",
        "--signer-address",
        "0x1234567890123456789012345678901234567890",
    ];

    /// Local signer CLI flags for tests that need a valid signing configuration.
    const LOCAL_SIGNER_ARGS: [&str; 2] =
        ["--private-key", "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"];

    #[test]
    fn test_valid_config() {
        let cli = cli_from_args(&SIGNER_ARGS);
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.game_type, 1);
        assert_eq!(config.poll_interval, Duration::from_secs(12));
        assert_eq!(config.zk_request_timeout, Duration::from_secs(30));
        assert_eq!(
            config.anchor_state_registry_addr,
            "0x2234567890123456789012345678901234567890".parse::<Address>().unwrap()
        );
        assert_eq!(config.bond_discovery_lookback_games, 1000);
        assert_eq!(config.bond_discovery_interval, Duration::from_secs(300));
        assert_eq!(config.health_addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert!(matches!(config.signing, SignerConfig::Remote { .. }));
        assert_eq!(config.tx_manager.num_confirmations, 10);
        assert_eq!(config.tx_manager.safe_abort_nonce_too_low_count, 3);
        assert_eq!(config.tx_manager.fee_limit_multiplier, 5);
    }

    #[test]
    fn test_bond_discovery_lookback_games_configurable() {
        let all_args = [&SIGNER_ARGS[..], &["--bond-discovery-lookback-games", "2048"]].concat();
        let cli = cli_from_args(&all_args);
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.bond_discovery_lookback_games, 2048);
    }

    #[rstest]
    #[case::poll_interval("--poll-interval", "0s", "poll-interval must be greater than 0")]
    #[case::zk_request_timeout(
        "--zk-request-timeout",
        "0s",
        "zk-request-timeout must be greater than 0"
    )]
    #[case::bond_discovery_lookback_games(
        "--bond-discovery-lookback-games",
        "0",
        "bond-discovery-lookback-games must be greater than 0"
    )]
    #[case::bond_discovery_interval(
        "--bond-discovery-interval",
        "0s",
        "bond-discovery-interval must be greater than 0"
    )]
    #[case::max_proof_duration(
        "--max-proof-duration",
        "0s",
        "max-proof-duration must be greater than 0"
    )]
    fn test_zero_value_rejected(#[case] flag: &str, #[case] value: &str, #[case] expected: &str) {
        let all_args = [&LOCAL_SIGNER_ARGS[..], &[flag, value]].concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        assert_eq!(result.unwrap_err().to_string(), expected);
    }

    #[test]
    fn test_health_port_zero_rejected() {
        let all_args = [&SIGNER_ARGS[..], &["--health.port", "0"]].concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        assert_eq!(result.unwrap_err().to_string(), "health.port must be greater than 0");
    }

    #[test]
    fn test_anchor_state_registry_zero_rejected() {
        let all_args = [
            &SIGNER_ARGS[..],
            &["--anchor-state-registry-addr", "0x0000000000000000000000000000000000000000"],
        ]
        .concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        assert_eq!(result.unwrap_err().to_string(), "anchor-state-registry-addr must be non-zero");
    }

    #[rstest]
    #[case::enabled(&["--metrics.enabled", "--metrics.port", "0"], true)]
    #[case::disabled(&["--metrics.port", "0"], false)]
    fn test_metrics_port_zero(#[case] args: &[&str], #[case] expect_error: bool) {
        let all_args = [args, &SIGNER_ARGS].concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        if expect_error {
            assert_eq!(
                result.unwrap_err().to_string(),
                "metrics.port must be greater than 0 when metrics are enabled"
            );
        } else {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn test_signing_config_local() {
        let cli = cli_from_args(&LOCAL_SIGNER_ARGS);
        let result = ChallengerConfig::from_cli(cli);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(matches!(result.unwrap().signing, SignerConfig::Local { .. }));
    }

    #[test]
    fn test_signing_config_remote() {
        let cli = cli_from_args(&SIGNER_ARGS);
        let result = ChallengerConfig::from_cli(cli);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(matches!(result.unwrap().signing, SignerConfig::Remote { .. }));
    }

    #[test]
    fn test_signing_config_none_provided() {
        let cli = cli_from_args(&[]);
        let result = ChallengerConfig::from_cli(cli);
        assert_eq!(result.unwrap_err().to_string(), "invalid signing config");
    }

    #[test]
    fn test_signing_config_conflicting_rejected_by_clap() {
        let result = Cli::try_parse_from([
            "challenger",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--dispute-game-factory-addr",
            "0x1234567890123456789012345678901234567890",
            "--anchor-state-registry-addr",
            "0x2234567890123456789012345678901234567890",
            "--game-type",
            "1",
            "--zk-rpc-url",
            "http://localhost:5000",
            "--private-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            "--signer-endpoint",
            "http://localhost:8546",
            "--signer-address",
            "0x1234567890123456789012345678901234567890",
        ]);
        assert!(result.is_err(), "clap should reject conflicting signer args");
    }

    #[test]
    fn test_signing_config_endpoint_without_address_rejected_by_clap() {
        let result = Cli::try_parse_from([
            "challenger",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--dispute-game-factory-addr",
            "0x1234567890123456789012345678901234567890",
            "--anchor-state-registry-addr",
            "0x2234567890123456789012345678901234567890",
            "--game-type",
            "1",
            "--zk-rpc-url",
            "http://localhost:5000",
            "--signer-endpoint",
            "http://localhost:8546",
        ]);
        assert!(result.is_err(), "clap should reject endpoint without address");
    }

    #[test]
    fn test_zk_rpc_url_validated() {
        let cli = cli_from_args(&[
            "--zk-rpc-url",
            "file:///no/host",
            "--private-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ]);
        let result = ChallengerConfig::from_cli(cli);
        assert_eq!(result.unwrap_err().to_string(), "invalid zk-rpc-url URL: missing host");
    }

    #[test]
    fn test_health_addr_configurable() {
        let args =
            [&SIGNER_ARGS[..], &["--health.addr", "127.0.0.1", "--health.port", "9090"]].concat();
        let cli = cli_from_args(&args);
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.health_addr, "127.0.0.1:9090".parse::<SocketAddr>().unwrap());
    }

    /// Like `cli_from_args` but omits the `--zk-rpc-url` default.
    fn cli_without_zk(extra_args: &[&str]) -> Cli {
        let base: &[(&str, &str)] = &[
            ("--l1-eth-rpc", "http://localhost:8545"),
            ("--l2-eth-rpc", "http://localhost:9545"),
            ("--dispute-game-factory-addr", "0x1234567890123456789012345678901234567890"),
            ("--anchor-state-registry-addr", "0x2234567890123456789012345678901234567890"),
            ("--game-type", "1"),
        ];

        let mut args = vec!["challenger"];
        for (key, value) in base {
            if !extra_args.contains(key) {
                args.push(key);
                args.push(value);
            }
        }
        args.extend_from_slice(extra_args);
        Cli::try_parse_from(args).unwrap()
    }

    const BOND_ADDR: &str = "0x1234567890123456789012345678901234567890";

    #[test]
    fn test_no_dispute_with_zk_rpc_url_rejected() {
        // `cli_from_args` includes `--zk-rpc-url` by default.
        let args = [&LOCAL_SIGNER_ARGS[..], &["--no-dispute", "--bond-claim-addresses", BOND_ADDR]]
            .concat();
        let result = ChallengerConfig::from_cli(cli_from_args(&args));
        assert_eq!(
            result.unwrap_err().to_string(),
            "--zk-rpc-url must not be set in --no-dispute mode"
        );
    }

    #[test]
    fn test_no_dispute_without_bond_addresses_rejected() {
        let args = [&LOCAL_SIGNER_ARGS[..], &["--no-dispute"]].concat();
        let result = ChallengerConfig::from_cli(cli_without_zk(&args));
        assert_eq!(
            result.unwrap_err().to_string(),
            "--bond-claim-addresses is required in --no-dispute mode"
        );
    }

    #[test]
    fn test_missing_zk_rpc_url_rejected_when_disputing() {
        let result = ChallengerConfig::from_cli(cli_without_zk(&LOCAL_SIGNER_ARGS));
        assert_eq!(
            result.unwrap_err().to_string(),
            "--zk-rpc-url is required unless --no-dispute is set"
        );
    }
}
