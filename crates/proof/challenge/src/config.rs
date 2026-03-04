//! Configuration types and validation for the challenger.

use std::{net::SocketAddr, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::{LogConfig, MetricsConfig};
use thiserror::Error;
use url::Url;
use zeroize::Zeroizing;

use crate::cli::Cli;

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
        /// The private key (hex-encoded), wrapped in [`Zeroizing`] for
        /// automatic memory zeroing on drop.
        private_key: Zeroizing<String>,
    },
    /// Remote signing via a signer sidecar JSON-RPC endpoint (production).
    Remote {
        /// URL of the signer sidecar.
        endpoint: Url,
        /// Address of the signer account.
        address: Address,
    },
}

impl std::fmt::Debug for SigningConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { .. } => {
                f.debug_struct("Local").field("private_key", &"[redacted]").finish()
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
    pub l1_eth_rpc: Url,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Url,
    /// URL of the rollup RPC endpoint.
    pub rollup_rpc: Url,
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory_addr: Address,
    /// Address of the `AnchorStateRegistry` contract on L1.
    pub anchor_state_registry_addr: Address,
    /// Game type ID for dispute games to monitor.
    pub game_type: u32,
    /// Polling interval for new dispute games.
    pub poll_interval: Duration,
    /// URL of the ZK proof service endpoint.
    pub zk_proof_service_endpoint: Url,
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
    /// # Validation
    ///
    /// - Every URL field must have a scheme and host.
    /// - `poll_interval` must be greater than zero.
    /// - When metrics are enabled, the metrics port must be non-zero.
    /// - Exactly one signing method must be configured: either
    ///   `CHALLENGER_PRIVATE_KEY` (local/dev) **or** both
    ///   `--signer-endpoint` and `--signer-address` (remote/production).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if any validation check fails.
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        // Validate URLs have scheme and host
        validate_url(&cli.challenger.l1_eth_rpc, "l1-eth-rpc")?;
        validate_url(&cli.challenger.l2_eth_rpc, "l2-eth-rpc")?;
        validate_url(&cli.challenger.rollup_rpc, "rollup-rpc")?;
        validate_url(&cli.challenger.zk_proof_service_endpoint, "zk-proof-service-endpoint")?;

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

        // Read private key from environment only — never accepted as a CLI argument
        // because command-line arguments are visible in process listings.
        let private_key = std::env::var("CHALLENGER_PRIVATE_KEY").ok();

        // Validate and extract signing config
        let signing = build_signing_config(
            private_key.as_deref(),
            cli.challenger.signer_endpoint.as_ref(),
            cli.challenger.signer_address.as_ref(),
        )?;

        let health_addr = SocketAddr::new(cli.challenger.health_addr, cli.challenger.health_port);

        Ok(Self {
            l1_eth_rpc: cli.challenger.l1_eth_rpc,
            l2_eth_rpc: cli.challenger.l2_eth_rpc,
            rollup_rpc: cli.challenger.rollup_rpc,
            dispute_game_factory_addr: cli.challenger.dispute_game_factory_addr,
            anchor_state_registry_addr: cli.challenger.anchor_state_registry_addr,
            game_type: cli.challenger.game_type,
            poll_interval: cli.challenger.poll_interval,
            zk_proof_service_endpoint: cli.challenger.zk_proof_service_endpoint,
            signing,
            lookback_games: cli.challenger.lookback_games,
            health_addr,
            log: LogConfig::from(cli.logging),
            metrics: cli.metrics.into(),
        })
    }
}

/// Validate that a URL has a scheme and host.
fn validate_url(url: &Url, field: &'static str) -> Result<(), ConfigError> {
    if url.scheme().is_empty() {
        return Err(ConfigError::InvalidUrl { field, reason: "missing scheme".to_string() });
    }

    if url.host().is_none() {
        return Err(ConfigError::InvalidUrl { field, reason: "missing host".to_string() });
    }

    Ok(())
}

/// Validate and build [`SigningConfig`] from CLI arguments.
///
/// Exactly one of `private_key` or (`signer_endpoint` + `signer_address`) must be provided.
fn build_signing_config(
    private_key: Option<&str>,
    signer_endpoint: Option<&Url>,
    signer_address: Option<&Address>,
) -> Result<SigningConfig, ConfigError> {
    match (private_key, signer_endpoint, signer_address) {
        (Some(pk), None, None) => {
            Ok(SigningConfig::Local { private_key: Zeroizing::new(pk.to_string()) })
        }
        (None, Some(endpoint), Some(address)) => {
            validate_url(endpoint, "signer-endpoint")?;
            Ok(SigningConfig::Remote { endpoint: endpoint.clone(), address: *address })
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
    use std::net::IpAddr;

    use base_cli_utils::LogFormat;

    use super::*;
    use crate::cli::{ChallengerArgs, LogArgs, MetricsArgs};

    fn minimal_cli() -> Cli {
        Cli {
            challenger: ChallengerArgs {
                l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
                l2_eth_rpc: Url::parse("http://localhost:9545").unwrap(),
                rollup_rpc: Url::parse("http://localhost:7545").unwrap(),
                dispute_game_factory_addr: "0x1234567890123456789012345678901234567890"
                    .parse()
                    .unwrap(),
                anchor_state_registry_addr: "0x2234567890123456789012345678901234567890"
                    .parse()
                    .unwrap(),
                game_type: 1,
                poll_interval: Duration::from_secs(12),
                zk_proof_service_endpoint: Url::parse("http://localhost:5000").unwrap(),
                signer_endpoint: Some(Url::parse("http://localhost:8546").unwrap()),
                signer_address: Some("0x1234567890123456789012345678901234567890".parse().unwrap()),
                lookback_games: 1000,
                health_addr: "0.0.0.0".parse().unwrap(),
                health_port: 8080,
            },
            logging: LogArgs {
                level: 3,
                stdout_quiet: false,
                stdout_format: LogFormat::Full,
                ..Default::default()
            },
            metrics: MetricsArgs {
                enabled: false,
                addr: "0.0.0.0".parse().unwrap(),
                port: 7310,
                ..Default::default()
            },
        }
    }

    #[test]
    fn test_valid_config() {
        let cli = minimal_cli();
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.game_type, 1);
        assert_eq!(config.poll_interval, Duration::from_secs(12));
        assert_eq!(config.lookback_games, 1000);
        assert_eq!(config.health_addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert!(matches!(config.signing, SigningConfig::Remote { .. }));
    }

    #[test]
    fn test_zero_poll_interval() {
        let mut cli = minimal_cli();
        cli.challenger.poll_interval = Duration::ZERO;
        let result = ChallengerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "poll-interval", .. })));
    }

    #[test]
    fn test_metrics_port_zero_when_enabled() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = true;
        cli.metrics.port = 0;
        let result = ChallengerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::Metrics(_))));
    }

    #[test]
    fn test_metrics_port_zero_when_disabled() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = false;
        cli.metrics.port = 0;
        let result = ChallengerConfig::from_cli(cli);
        assert!(result.is_ok());
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
        let result = validate_url(&url, "test-field");
        assert!(matches!(result, Err(ConfigError::InvalidUrl { field: "test-field", .. })));
    }

    #[test]
    fn test_config_error_display() {
        let error =
            ConfigError::InvalidUrl { field: "l1-eth-rpc", reason: "missing host".to_string() };
        assert_eq!(error.to_string(), "invalid l1-eth-rpc URL: missing host");

        let error = ConfigError::OutOfRange {
            field: "poll-interval",
            constraint: "greater than 0",
            value: "0".to_string(),
        };
        assert_eq!(error.to_string(), "poll-interval must be greater than 0, got 0");

        let error = ConfigError::Metrics("port must be non-zero".to_string());
        assert_eq!(error.to_string(), "invalid metrics config: port must be non-zero");

        let error = ConfigError::Signing("missing key".to_string());
        assert_eq!(error.to_string(), "invalid signing config: missing key");
    }

    #[test]
    fn test_signing_config_local() {
        let pk = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signing = build_signing_config(Some(pk), None, None).unwrap();
        assert!(matches!(signing, SigningConfig::Local { .. }));
    }

    #[test]
    fn test_signing_config_remote() {
        let url = Url::parse("http://localhost:8546").unwrap();
        let addr: Address = "0x1234567890123456789012345678901234567890".parse().unwrap();
        let signing = build_signing_config(None, Some(&url), Some(&addr)).unwrap();
        assert!(matches!(signing, SigningConfig::Remote { .. }));
    }

    #[test]
    fn test_signing_config_none_provided() {
        let result = build_signing_config(None, None, None);
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_signing_config_both_provided() {
        let pk = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let url = Url::parse("http://localhost:8546").unwrap();
        let result = build_signing_config(Some(pk), Some(&url), None);
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_signing_config_endpoint_without_address() {
        let url = Url::parse("http://localhost:8546").unwrap();
        let result = build_signing_config(None, Some(&url), None);
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_zk_proof_endpoint_validated() {
        let mut cli = minimal_cli();
        cli.challenger.zk_proof_service_endpoint = Url::parse("file:///no/host").unwrap();
        let result = ChallengerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::InvalidUrl { field: "zk-proof-service-endpoint", .. })
        ));
    }

    #[test]
    fn test_health_addr_configurable() {
        let mut cli = minimal_cli();
        cli.challenger.health_addr = "127.0.0.1".parse::<IpAddr>().unwrap();
        cli.challenger.health_port = 9090;
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.health_addr, "127.0.0.1:9090".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn test_signing_config_debug_redacts() {
        let signing =
            SigningConfig::Local { private_key: Zeroizing::new("0xdeadbeef".to_string()) };
        let debug_output = format!("{signing:?}");
        assert!(debug_output.contains("[redacted]"));
        assert!(!debug_output.contains("deadbeef"));
    }
}
