//! Configuration types and validation for the challenger.

use std::{fmt, net::SocketAddr, ops::Deref, time::Duration};

use alloy_primitives::Address;
use alloy_signer::k256::ecdsa::SigningKey;
use alloy_signer_local::PrivateKeySigner;
use base_cli_utils::{LogConfig, MetricsConfig};
use thiserror::Error;
use url::Url;
use zeroize::Zeroizing;

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
    #[error("invalid {field} URL: {reason}")]
    InvalidUrl {
        /// The field name that contains the invalid URL.
        field: &'static str,
        /// The reason the URL is invalid.
        reason: String,
    },
    /// A field value is out of the allowed range.
    #[error("{field} must be {constraint}, got {value}")]
    OutOfRange {
        /// The field name that is out of range.
        field: &'static str,
        /// The constraint description.
        constraint: &'static str,
        /// The actual value.
        value: String,
    },
    /// Invalid metrics configuration.
    #[error("invalid metrics config: {0}")]
    Metrics(String),
    /// Invalid signing configuration.
    #[error("invalid signing config: {0}")]
    Signing(String),
}

/// Signing configuration for L1 transaction submission.
#[derive(Clone)]
pub enum SigningConfig {
    /// Local signing with an in-process private key (development).
    Local {
        /// The parsed private-key signer, ready for transaction signing.
        signer: PrivateKeySigner,
    },
    /// Remote signing via a signer sidecar JSON-RPC endpoint (production).
    Remote {
        /// URL of the signer sidecar.
        endpoint: Validated<Url>,
        /// Address of the signer account.
        address: Address,
    },
}

impl std::fmt::Debug for SigningConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { signer } => {
                f.debug_struct("Local").field("address", &signer.address()).finish()
            }
            Self::Remote { endpoint, address } => f
                .debug_struct("Remote")
                .field("endpoint", endpoint)
                .field("address", address)
                .finish(),
        }
    }
}

/// Validated challenger configuration.
#[derive(Debug, Clone)]
pub struct ChallengerConfig {
    /// URL of the L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Validated<Url>,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Validated<Url>,
    /// URL of the rollup RPC endpoint.
    pub rollup_rpc: Validated<Url>,
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory_addr: Address,
    /// Address of the `AnchorStateRegistry` contract on L1.
    pub anchor_state_registry_addr: Address,
    /// Game type ID for dispute games to monitor.
    pub game_type: u32,
    /// Polling interval for new dispute games.
    pub poll_interval: Duration,
    /// URL of the ZK proof service endpoint.
    pub zk_proof_service_endpoint: Validated<Url>,
    /// Signing configuration for L1 transaction submission.
    pub signing: SigningConfig,
    /// Number of past games to scan on startup.
    pub lookback_games: u64,
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
    /// `private_key` is the raw hex private key (with or without `0x` prefix)
    /// read from the `CHALLENGER_PRIVATE_KEY` environment variable by the
    /// binary entrypoint. Passing it explicitly keeps this function pure and
    /// free of process-global state.
    ///
    /// # Validation
    ///
    /// - Every URL field must have a scheme and host.
    /// - `poll_interval` must be greater than zero.
    /// - When metrics are enabled, the metrics port must be non-zero.
    /// - Exactly one signing method must be configured: either
    ///   `private_key` (local/dev) **or** both
    ///   `--signer-endpoint` and `--signer-address` (remote/production).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if any validation check fails.
    pub fn from_cli(cli: Cli, private_key: Option<String>) -> Result<Self, ConfigError> {
        let validate = |url: Url, field: &'static str| -> Result<Validated<Url>, ConfigError> {
            Validated::try_from(url)
                .map_err(|e| ConfigError::InvalidUrl { field, reason: e.to_string() })
        };

        // Validate URLs have scheme and host
        let l1_eth_rpc = validate(cli.challenger.l1_eth_rpc, "l1-eth-rpc")?;
        let l2_eth_rpc = validate(cli.challenger.l2_eth_rpc, "l2-eth-rpc")?;
        let rollup_rpc = validate(cli.challenger.rollup_rpc, "rollup-rpc")?;
        let zk_proof_service_endpoint =
            validate(cli.challenger.zk_proof_service_endpoint, "zk-proof-service-endpoint")?;

        // Validate poll_interval > 0
        if cli.challenger.poll_interval.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "poll-interval",
                constraint: "greater than 0",
                value: "0".to_string(),
            });
        }

        // Validate metrics port when enabled
        if cli.metrics.enabled && cli.metrics.port == 0 {
            return Err(ConfigError::Metrics(
                "metrics port must be non-zero when metrics are enabled".to_string(),
            ));
        }

        // Validate and extract signing config
        let signing = build_signing_config(
            private_key.as_deref(),
            cli.challenger.signer_endpoint,
            cli.challenger.signer_address.as_ref(),
        )?;

        let health_addr = SocketAddr::new(cli.challenger.health_addr, cli.challenger.health_port);

        Ok(Self {
            l1_eth_rpc,
            l2_eth_rpc,
            rollup_rpc,
            dispute_game_factory_addr: cli.challenger.dispute_game_factory_addr,
            anchor_state_registry_addr: cli.challenger.anchor_state_registry_addr,
            game_type: cli.challenger.game_type,
            poll_interval: cli.challenger.poll_interval,
            zk_proof_service_endpoint,
            signing,
            lookback_games: cli.challenger.lookback_games,
            health_addr,
            log: LogConfig::from(cli.logging),
            metrics: cli.metrics.into(),
        })
    }
}

/// Validate and build [`SigningConfig`] from CLI arguments.
///
/// Exactly one of `private_key` or (`signer_endpoint` + `signer_address`) must be provided.
fn build_signing_config(
    private_key: Option<&str>,
    signer_endpoint: Option<Url>,
    signer_address: Option<&Address>,
) -> Result<SigningConfig, ConfigError> {
    match (private_key, signer_endpoint, signer_address) {
        (Some(pk), None, None) => {
            let hex_str = pk.strip_prefix("0x").unwrap_or(pk);
            let key_bytes = Zeroizing::new(
                hex::decode(hex_str)
                    .map_err(|e| ConfigError::Signing(format!("invalid private key hex: {e}")))?,
            );
            let signing_key = SigningKey::from_slice(&key_bytes)
                .map_err(|e| ConfigError::Signing(format!("invalid private key: {e}")))?;
            let signer = PrivateKeySigner::from_signing_key(signing_key);
            Ok(SigningConfig::Local { signer })
        }
        (None, Some(endpoint), Some(address)) => {
            let endpoint =
                Validated::try_from(endpoint).map_err(|e| ConfigError::InvalidUrl {
                    field: "signer-endpoint",
                    reason: e.to_string(),
                })?;
            Ok(SigningConfig::Remote { endpoint, address: *address })
        }
        (None, None, None) => Err(ConfigError::Signing(
            "one of CHALLENGER_PRIVATE_KEY or (--signer-endpoint + --signer-address) must be provided"
                .to_string(),
        )),
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(ConfigError::Signing(
            "CHALLENGER_PRIVATE_KEY is mutually exclusive with --signer-endpoint/--signer-address"
                .to_string(),
        )),
        (None, Some(_), None) => {
            Err(ConfigError::Signing("--signer-endpoint requires --signer-address".to_string()))
        }
        (None, None, Some(_)) => {
            Err(ConfigError::Signing("--signer-address requires --signer-endpoint".to_string()))
        }
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
    /// Keys present in `extra_args` replace their base defaults so clap never
    /// sees the same flag twice.
    fn cli_from_args(extra_args: &[&str]) -> Cli {
        let base: &[(&str, &str)] = &[
            ("--l1-eth-rpc", "http://localhost:8545"),
            ("--l2-eth-rpc", "http://localhost:9545"),
            ("--rollup-rpc", "http://localhost:7545"),
            ("--dispute-game-factory-addr", "0x1234567890123456789012345678901234567890"),
            ("--anchor-state-registry-addr", "0x2234567890123456789012345678901234567890"),
            ("--game-type", "1"),
            ("--zk-proof-service-endpoint", "http://localhost:5000"),
            ("--signer-endpoint", "http://localhost:8546"),
            ("--signer-address", "0x1234567890123456789012345678901234567890"),
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

    #[test]
    fn test_valid_config() {
        let cli = cli_from_args(&[]);
        let config = ChallengerConfig::from_cli(cli, None).unwrap();
        assert_eq!(config.game_type, 1);
        assert_eq!(config.poll_interval, Duration::from_secs(12));
        assert_eq!(config.lookback_games, 1000);
        assert_eq!(config.health_addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert!(matches!(config.signing, SigningConfig::Remote { .. }));
    }

    #[test]
    fn test_zero_poll_interval() {
        let cli = cli_from_args(&["--poll-interval", "0s"]);
        let result = ChallengerConfig::from_cli(cli, None);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "poll-interval", .. })));
    }

    #[rstest]
    #[case::enabled(&["--metrics.enabled", "--metrics.port", "0"], true)]
    #[case::disabled(&["--metrics.port", "0"], false)]
    fn test_metrics_port_zero(#[case] args: &[&str], #[case] expect_error: bool) {
        let cli = cli_from_args(args);
        let result = ChallengerConfig::from_cli(cli, None);
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
        ConfigError::InvalidUrl { field: "l1-eth-rpc", reason: "missing host".to_string() },
        "invalid l1-eth-rpc URL: missing host"
    )]
    #[case::out_of_range(
        ConfigError::OutOfRange { field: "poll-interval", constraint: "greater than 0", value: "0".to_string() },
        "poll-interval must be greater than 0, got 0"
    )]
    #[case::metrics(
        ConfigError::Metrics("port must be non-zero".to_string()),
        "invalid metrics config: port must be non-zero"
    )]
    #[case::signing(
        ConfigError::Signing("missing key".to_string()),
        "invalid signing config: missing key"
    )]
    fn test_config_error_display(#[case] error: ConfigError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[rstest]
    #[case::local(
        Some("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
        None,
        None,
        true
    )]
    #[case::remote(
        None,
        Some("http://localhost:8546"),
        Some("0x1234567890123456789012345678901234567890"),
        true
    )]
    #[case::none_provided(None, None, None, false)]
    #[case::both_provided(
        Some("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
        Some("http://localhost:8546"),
        None,
        false
    )]
    #[case::endpoint_without_address(None, Some("http://localhost:8546"), None, false)]
    fn test_signing_config(
        #[case] pk: Option<&str>,
        #[case] url_str: Option<&str>,
        #[case] addr_str: Option<&str>,
        #[case] should_succeed: bool,
    ) {
        let url = url_str.map(|s| Url::parse(s).unwrap());
        let addr: Option<Address> = addr_str.map(|s| s.parse().unwrap());
        let result = build_signing_config(pk, url, addr.as_ref());
        if should_succeed {
            assert!(result.is_ok(), "expected Ok, got {result:?}");
        } else {
            assert!(matches!(result, Err(ConfigError::Signing(_))));
        }
    }

    #[test]
    fn test_zk_proof_endpoint_validated() {
        let cli = cli_from_args(&["--zk-proof-service-endpoint", "file:///no/host"]);
        let result = ChallengerConfig::from_cli(cli, None);
        assert!(matches!(
            result,
            Err(ConfigError::InvalidUrl { field: "zk-proof-service-endpoint", .. })
        ));
    }

    #[test]
    fn test_health_addr_configurable() {
        let cli = cli_from_args(&["--health.addr", "127.0.0.1", "--health.port", "9090"]);
        let config = ChallengerConfig::from_cli(cli, None).unwrap();
        assert_eq!(config.health_addr, "127.0.0.1:9090".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn test_signing_config_debug_shows_address() {
        let pk = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signing = build_signing_config(Some(pk), None, None).unwrap();
        let debug_output = format!("{signing:?}");
        assert!(debug_output.contains("address"));
        assert!(
            !debug_output
                .contains("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        );
    }
}
