//! Configuration types and validation for the challenger.

use std::{fmt, net::SocketAddr, ops::Deref, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::{LogConfig, MetricsConfig};
use base_tx_manager::{SignerConfig, TxManagerConfig};
use thiserror::Error;
use url::Url;

use crate::cli::Cli;

/// Error returned when URL validation fails.
#[derive(Debug, Error)]
#[error("missing host")]
pub struct UrlValidationError;

/// A wrapper that guarantees the inner value has been validated.
#[derive(Debug, Clone)]
pub struct Validated<T>(T);

impl TryFrom<Url> for Validated<Url> {
    type Error = UrlValidationError;

    fn try_from(url: Url) -> Result<Self, Self::Error> {
        if url.host().is_none() {
            return Err(UrlValidationError);
        }
        Ok(Self(url))
    }
}

impl<T> Deref for Validated<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> AsRef<T> for Validated<T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T: fmt::Display> fmt::Display for Validated<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Errors that can occur during configuration validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Invalid URL format.
    #[error("invalid {field} URL: missing host")]
    InvalidUrl {
        /// The field name that contains the invalid URL.
        field: &'static str,
    },
    /// A field value is out of the allowed range.
    #[error("{field} must be {constraint}, got {value}")]
    OutOfRange {
        /// The field name that is out of range.
        field: &'static str,
        /// The constraint description.
        constraint: &'static str,
        /// The actual value.
        value: &'static str,
    },
    /// Invalid metrics configuration.
    #[error("invalid metrics config: {0}")]
    Metrics(&'static str),
    /// Invalid signing configuration.
    #[error("invalid signing config: {0}")]
    Signer(base_tx_manager::ConfigError),
    /// Invalid transaction manager configuration.
    #[error("invalid tx manager config: {0}")]
    TxManager(base_tx_manager::ConfigError),
}

/// Validated challenger configuration.
#[derive(Debug)]
pub struct ChallengerConfig {
    /// URL of the L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Validated<Url>,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Validated<Url>,
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory_addr: Address,
    /// Polling interval for new dispute games.
    pub poll_interval: Duration,
    /// URL of the ZK RPC endpoint.
    pub zk_rpc_url: Validated<Url>,
    /// Timeout for establishing the initial gRPC connection to the ZK proof service.
    pub zk_connect_timeout: Duration,
    /// Timeout for individual gRPC requests to the ZK proof service.
    pub zk_request_timeout: Duration,
    /// URL of the TEE enclave RPC endpoint (optional).
    pub tee_rpc_url: Option<Validated<Url>>,
    /// Timeout for individual TEE proof requests (only `Some` when TEE is enabled).
    pub tee_request_timeout: Option<Duration>,
    /// Signing configuration for L1 transaction submission.
    pub signing: SignerConfig,
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    pub tx_manager: TxManagerConfig,
    /// Number of past games to scan on startup.
    pub lookback_games: u64,
    /// How often a full rescan of the bond lookback window is performed.
    pub bond_discovery_interval: Duration,
    /// Maximum time to keep a completed bond game tracked while waiting for
    /// its anchor update to complete.
    pub anchor_update_retention: Duration,
    /// Addresses to claim bonds on behalf of.
    pub bond_claim_addresses: Vec<Address>,
    /// Health server socket address.
    pub health_addr: SocketAddr,
    /// Logging configuration (from base-cli-utils).
    pub log: LogConfig,
    /// Metrics server configuration.
    pub metrics: MetricsConfig,
}

impl ChallengerConfig {
    /// Creates a validated [`ChallengerConfig`] from parsed CLI arguments.
    ///
    /// # Validation
    ///
    /// - Every URL field must have a scheme and host.
    /// - `poll_interval` must be greater than zero.
    /// - When metrics are enabled, the metrics port must be non-zero.
    /// - Exactly one signing method must be configured: either
    ///   `--private-key` (local/dev) **or** both
    ///   `--signer-endpoint` and `--signer-address` (remote/production).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if any validation check fails.
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        let validate_url = |url: Url, field: &'static str| -> Result<Validated<Url>, ConfigError> {
            Validated::try_from(url).map_err(|_| ConfigError::InvalidUrl { field })
        };

        let require_nonzero = |value: u64, field: &'static str| -> Result<(), ConfigError> {
            if value == 0 {
                return Err(ConfigError::OutOfRange {
                    field,
                    constraint: "greater than 0",
                    value: "0",
                });
            }
            Ok(())
        };

        let require_nonzero_duration =
            |d: Duration, field: &'static str| -> Result<(), ConfigError> {
                if d.is_zero() {
                    return Err(ConfigError::OutOfRange {
                        field,
                        constraint: "greater than 0",
                        value: "0",
                    });
                }
                Ok(())
            };

        let l1_eth_rpc = validate_url(cli.challenger.l1_eth_rpc, "l1-eth-rpc")?;
        let l2_eth_rpc = validate_url(cli.challenger.l2_eth_rpc, "l2-eth-rpc")?;
        let zk_rpc_url = validate_url(cli.challenger.zk_rpc_url, "zk-rpc-url")?;
        let tee_rpc_url =
            cli.challenger.tee_rpc_url.map(|url| validate_url(url, "tee-rpc-url")).transpose()?;

        require_nonzero_duration(cli.challenger.poll_interval, "poll-interval")?;
        require_nonzero_duration(cli.challenger.zk_connect_timeout, "zk-connect-timeout")?;
        require_nonzero_duration(cli.challenger.zk_request_timeout, "zk-request-timeout")?;

        let tee_request_timeout = if tee_rpc_url.is_some() {
            require_nonzero_duration(cli.challenger.tee_request_timeout, "tee-request-timeout")?;
            Some(cli.challenger.tee_request_timeout)
        } else {
            None
        };

        require_nonzero(cli.challenger.lookback_games, "lookback-games")?;
        require_nonzero_duration(
            cli.challenger.bond_discovery_interval,
            "bond-discovery-interval",
        )?;
        require_nonzero_duration(
            cli.challenger.anchor_update_retention,
            "anchor-update-retention",
        )?;

        // Health server is always started, so the port must be valid.
        require_nonzero(cli.health.port.into(), "health.port")?;

        if cli.metrics.enabled && cli.metrics.port == 0 {
            return Err(ConfigError::Metrics(
                "metrics port must be non-zero when metrics are enabled",
            ));
        }

        let signing = SignerConfig::try_from(cli.challenger.signer).map_err(ConfigError::Signer)?;

        let tx_manager =
            TxManagerConfig::try_from(cli.challenger.tx_manager).map_err(ConfigError::TxManager)?;

        let health_addr = cli.health.socket_addr();

        Ok(Self {
            l1_eth_rpc,
            l2_eth_rpc,
            dispute_game_factory_addr: cli.challenger.dispute_game_factory_addr,
            poll_interval: cli.challenger.poll_interval,
            zk_rpc_url,
            zk_connect_timeout: cli.challenger.zk_connect_timeout,
            zk_request_timeout: cli.challenger.zk_request_timeout,
            tee_rpc_url,
            tee_request_timeout,
            signing,
            tx_manager,
            lookback_games: cli.challenger.lookback_games,
            bond_discovery_interval: cli.challenger.bond_discovery_interval,
            anchor_update_retention: cli.challenger.anchor_update_retention,
            bond_claim_addresses: cli.challenger.bond_claim_addresses,
            health_addr,
            log: LogConfig::from(cli.logging),
            metrics: cli.metrics.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use base_cli_utils::LogFormat;
    use clap::Parser;
    use rstest::rstest;

    use super::*;
    use crate::cli::{LogArgs, MetricsArgs};

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
        assert_eq!(config.poll_interval, Duration::from_secs(12));
        assert_eq!(config.zk_connect_timeout, Duration::from_secs(10));
        assert_eq!(config.zk_request_timeout, Duration::from_secs(30));
        assert_eq!(config.lookback_games, 1000);
        assert_eq!(config.bond_discovery_interval, Duration::from_secs(300));
        assert_eq!(config.anchor_update_retention, Duration::from_secs(24 * 60 * 60));
        assert_eq!(config.health_addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert!(matches!(config.signing, SignerConfig::Remote { .. }));
        assert_eq!(config.tx_manager.num_confirmations, 10);
        assert_eq!(config.tx_manager.safe_abort_nonce_too_low_count, 3);
        assert_eq!(config.tx_manager.fee_limit_multiplier, 5);
    }

    #[rstest]
    #[case::poll_interval("--poll-interval", "0s", "poll-interval")]
    #[case::zk_connect_timeout("--zk-connect-timeout", "0s", "zk-connect-timeout")]
    #[case::zk_request_timeout("--zk-request-timeout", "0s", "zk-request-timeout")]
    #[case::lookback_games("--lookback-games", "0", "lookback-games")]
    #[case::bond_discovery_interval("--bond-discovery-interval", "0s", "bond-discovery-interval")]
    #[case::anchor_update_retention("--anchor-update-retention", "0s", "anchor-update-retention")]
    fn test_zero_value_rejected(#[case] flag: &str, #[case] value: &str, #[case] field: &str) {
        let all_args = [&LOCAL_SIGNER_ARGS[..], &[flag, value]].concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: f, .. }) if f == field));
    }

    #[test]
    fn test_health_port_zero_rejected() {
        let all_args = [&SIGNER_ARGS[..], &["--health.port", "0"]].concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "health.port", .. })));
    }

    #[rstest]
    #[case::enabled(&["--metrics.enabled", "--metrics.port", "0"], true)]
    #[case::disabled(&["--metrics.port", "0"], false)]
    fn test_metrics_port_zero(#[case] args: &[&str], #[case] expect_error: bool) {
        let all_args = [args, &SIGNER_ARGS].concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        if expect_error {
            assert!(matches!(result, Err(ConfigError::Metrics(_))));
        } else {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn test_log_config_from_args() {
        use tracing::level_filters::LevelFilter;

        let args = LogArgs {
            level: 4,
            stdout_quiet: false,
            stdout_format: LogFormat::Json,
            ..Default::default()
        };
        let config = LogConfig::from(args);
        assert_eq!(config.global_level, LevelFilter::DEBUG);
        assert!(config.stdout_logs.is_some());
        assert!(config.file_logs.is_none());

        let args = LogArgs {
            level: 3,
            stdout_quiet: true,
            stdout_format: LogFormat::Full,
            ..Default::default()
        };
        let config = LogConfig::from(args);
        assert!(config.stdout_logs.is_none());
    }

    #[test]
    fn test_metrics_config_from_args() {
        let args = MetricsArgs {
            enabled: true,
            addr: "127.0.0.1".parse().unwrap(),
            port: 9090,
            ..Default::default()
        };
        let config = MetricsConfig::from(args);
        assert!(config.enabled);
        assert_eq!(config.port, 9090);
    }

    #[test]
    fn test_url_without_host() {
        let url = Url::parse("file:///some/path").unwrap();
        let result = Validated::try_from(url);
        assert!(matches!(result, Err(UrlValidationError)));
    }

    #[rstest]
    #[case::invalid_url(
        ConfigError::InvalidUrl { field: "l1-eth-rpc" },
        "invalid l1-eth-rpc URL: missing host"
    )]
    #[case::out_of_range(
        ConfigError::OutOfRange { field: "poll-interval", constraint: "greater than 0", value: "0" },
        "poll-interval must be greater than 0, got 0"
    )]
    #[case::metrics(
        ConfigError::Metrics("port must be non-zero"),
        "invalid metrics config: port must be non-zero"
    )]
    fn test_config_error_display(#[case] error: ConfigError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
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
        assert!(matches!(result, Err(ConfigError::Signer(_))));
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
        assert!(matches!(result, Err(ConfigError::InvalidUrl { field: "zk-rpc-url", .. })));
    }

    #[test]
    fn test_zero_tee_request_timeout_rejected_when_tee_enabled() {
        let all_args = [
            &LOCAL_SIGNER_ARGS[..],
            &["--tee-rpc-url", "http://localhost:9999", "--tee-request-timeout", "0s"],
        ]
        .concat();
        let cli = cli_from_args(&all_args);
        let result = ChallengerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange { field: "tee-request-timeout", .. })
        ));
    }

    #[test]
    fn test_health_addr_configurable() {
        let args =
            [&SIGNER_ARGS[..], &["--health.addr", "127.0.0.1", "--health.port", "9090"]].concat();
        let cli = cli_from_args(&args);
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.health_addr, "127.0.0.1:9090".parse::<SocketAddr>().unwrap());
    }
}
