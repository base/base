//! Configuration types and validation for the proposer.

use std::{net::IpAddr, time::Duration};

use alloy_primitives::{Address, B256};
use base_cli_utils::{LogConfig, MetricsConfig};
use base_proof_rpc::RetryConfig;
use thiserror::Error;
use url::Url;

use crate::cli::{Cli, ProposerArgs, RpcServerArgs};

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
    /// Invalid RPC configuration.
    #[error("invalid RPC config: {0}")]
    Rpc(String),
    /// Invalid signing configuration.
    #[error("invalid signing config: {0}")]
    Signing(String),
    /// Invalid transaction manager configuration.
    #[error("invalid tx manager config: {0}")]
    TxManager(String),
}

/// Validated proposer configuration.
#[derive(Debug)]
pub struct ProposerConfig {
    /// Allow proposals based on non-finalized L1 data.
    pub allow_non_finalized: bool,
    /// URL of the enclave RPC endpoint.
    pub enclave_rpc: Url,
    /// URL of the L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Url,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Url,
    /// Use reth-specific RPC calls for L2.
    pub l2_reth: bool,
    /// Address of the `AnchorStateRegistry` contract on L1.
    pub anchor_state_registry_addr: Address,
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory_addr: Address,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// Keccak256 hash of the TEE image PCR0.
    pub tee_image_hash: B256,
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// RPC request timeout.
    pub rpc_timeout: Duration,
    /// URL of the rollup RPC endpoint.
    pub rollup_rpc: Url,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: bool,
    /// Wait for node sync before starting.
    pub wait_node_sync: bool,
    /// Logging configuration (from base-cli-utils).
    pub log: LogConfig,
    /// Metrics server configuration.
    pub metrics: MetricsConfig,
    /// RPC server configuration.
    pub rpc: RpcServerConfig,
    /// RPC retry configuration.
    pub retry: RetryConfig,
    /// Signing configuration for L1 transaction submission.
    pub signing: base_tx_manager::SignerConfig,
    /// Transaction manager configuration.
    pub tx_manager: base_tx_manager::TxManagerConfig,
}

impl ProposerConfig {
    /// Create a validated configuration from CLI arguments.
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        // Validate URLs have scheme and host
        validate_url(&cli.proposer.enclave_rpc, "enclave-rpc")?;
        validate_url(&cli.proposer.l1_eth_rpc, "l1-eth-rpc")?;
        validate_url(&cli.proposer.l2_eth_rpc, "l2-eth-rpc")?;

        validate_url(&cli.proposer.rollup_rpc, "rollup-rpc")?;

        // Validate poll_interval > 0
        if cli.proposer.poll_interval.is_zero() {
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

        // Validate RPC port when admin is enabled
        if cli.rpc.enable_admin && cli.rpc.port == 0 {
            return Err(ConfigError::Rpc(
                "RPC port must be non-zero when admin is enabled".to_string(),
            ));
        }

        // Extract retry config before moving signer out of proposer
        let retry = RetryConfig::from(&cli.proposer);

        // Validate and extract signing config
        let signing = base_tx_manager::SignerConfig::try_from(cli.proposer.signer)
            .map_err(|e| ConfigError::Signing(e.to_string()))?;

        // Validate and extract tx manager config
        let tx_manager = base_tx_manager::TxManagerConfig::try_from(cli.proposer.tx_manager)
            .map_err(|e| ConfigError::TxManager(e.to_string()))?;

        Ok(Self {
            allow_non_finalized: cli.proposer.allow_non_finalized,
            enclave_rpc: cli.proposer.enclave_rpc,
            l1_eth_rpc: cli.proposer.l1_eth_rpc,
            l2_eth_rpc: cli.proposer.l2_eth_rpc,
            l2_reth: cli.proposer.l2_reth,
            anchor_state_registry_addr: cli.proposer.anchor_state_registry_addr,
            dispute_game_factory_addr: cli.proposer.dispute_game_factory_addr,
            game_type: cli.proposer.game_type,
            tee_image_hash: cli.proposer.tee_image_hash,
            poll_interval: cli.proposer.poll_interval,
            rpc_timeout: cli.proposer.rpc_timeout,
            retry,
            signing,
            tx_manager,
            rollup_rpc: cli.proposer.rollup_rpc,
            skip_tls_verify: cli.proposer.skip_tls_verify,
            wait_node_sync: cli.proposer.wait_node_sync,
            log: LogConfig::from(cli.logging),
            metrics: cli.metrics.into(),
            rpc: RpcServerConfig::from(cli.rpc),
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

/// Validated RPC server configuration.
#[derive(Debug, Clone)]
pub struct RpcServerConfig {
    /// Whether admin RPC methods are enabled.
    pub enable_admin: bool,
    /// RPC server bind address.
    pub addr: IpAddr,
    /// RPC server port.
    pub port: u16,
}

impl From<RpcServerArgs> for RpcServerConfig {
    fn from(args: RpcServerArgs) -> Self {
        Self { enable_admin: args.enable_admin, addr: args.addr, port: args.port }
    }
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self { enable_admin: false, addr: "127.0.0.1".parse().unwrap(), port: 8545 }
    }
}

impl From<&ProposerArgs> for RetryConfig {
    fn from(args: &ProposerArgs) -> Self {
        Self {
            max_attempts: args.rpc_max_retries,
            initial_delay: args.rpc_retry_initial_delay,
            max_delay: args.rpc_retry_max_delay,
        }
    }
}

#[cfg(test)]
mod tests {
    use base_cli_utils::LogFormat;

    use super::*;
    use crate::cli::{Cli, LogArgs, MetricsArgs, ProposerArgs, SignerCli, TxManagerCli};

    fn minimal_cli() -> Cli {
        Cli {
            proposer: ProposerArgs {
                allow_non_finalized: false,
                enclave_rpc: Url::parse("http://localhost:8080").unwrap(),
                l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
                l2_eth_rpc: Url::parse("http://localhost:9545").unwrap(),
                l2_reth: false,
                anchor_state_registry_addr: "0x1234567890123456789012345678901234567890"
                    .parse()
                    .unwrap(),
                dispute_game_factory_addr: "0x2234567890123456789012345678901234567890"
                    .parse()
                    .unwrap(),
                game_type: 1,
                tee_image_hash: B256::repeat_byte(0x01),
                poll_interval: Duration::from_secs(12),
                rpc_timeout: Duration::from_secs(30),
                rollup_rpc: Url::parse("http://localhost:7545").unwrap(),
                skip_tls_verify: false,
                wait_node_sync: false,
                rpc_max_retries: 5,
                rpc_retry_initial_delay: Duration::from_millis(100),
                rpc_retry_max_delay: Duration::from_secs(10),
                signer: SignerCli {
                    private_key: Some(
                        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                            .to_string(),
                    ),
                    signer_endpoint: None,
                    signer_address: None,
                },
                tx_manager: TxManagerCli::default(),
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
                port: 7300,
                ..Default::default()
            },
            rpc: RpcServerArgs {
                enable_admin: false,
                addr: "127.0.0.1".parse().unwrap(),
                port: 8545,
            },
        }
    }

    #[test]
    fn test_valid_config() {
        let cli = minimal_cli();
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(!config.allow_non_finalized);
        assert_eq!(config.game_type, 1);
        assert_eq!(config.poll_interval, Duration::from_secs(12));
        assert_eq!(config.rpc_timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_zero_poll_interval() {
        let mut cli = minimal_cli();
        cli.proposer.poll_interval = Duration::ZERO;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "poll-interval", .. })));
    }

    #[test]
    fn test_metrics_port_zero_when_enabled() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = true;
        cli.metrics.port = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::Metrics(_))));
    }

    #[test]
    fn test_metrics_port_zero_when_disabled() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = false;
        cli.metrics.port = 0;
        // Should be fine since metrics are disabled
        let result = ProposerConfig::from_cli(cli);
        assert!(result.is_ok());
    }

    #[test]
    fn test_rpc_port_zero_when_admin_enabled() {
        let mut cli = minimal_cli();
        cli.rpc.enable_admin = true;
        cli.rpc.port = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::Rpc(_))));
    }

    #[test]
    fn test_rpc_port_zero_when_admin_disabled() {
        let mut cli = minimal_cli();
        cli.rpc.enable_admin = false;
        cli.rpc.port = 0;
        // Should be fine since admin is disabled
        let result = ProposerConfig::from_cli(cli);
        assert!(result.is_ok());
    }

    #[test]
    fn test_log_config_from_args() {
        use tracing::level_filters::LevelFilter;

        // Test verbosity level 4 (DEBUG)
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

        // Test stdout_quiet suppresses stdout logging
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
    fn test_rpc_server_config_from_args() {
        let args =
            RpcServerArgs { enable_admin: true, addr: "0.0.0.0".parse().unwrap(), port: 8080 };
        let config = RpcServerConfig::from(args);
        assert!(config.enable_admin);
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_url_without_host() {
        // Create URL that parses but has no host (file:// URLs for instance)
        let url = Url::parse("file:///some/path").unwrap();
        let result = validate_url(&url, "test-field");
        assert!(matches!(result, Err(ConfigError::InvalidUrl { field: "test-field", .. })));
    }

    #[test]
    fn test_config_error_display() {
        let error =
            ConfigError::InvalidUrl { field: "enclave-rpc", reason: "missing host".to_string() };
        assert_eq!(error.to_string(), "invalid enclave-rpc URL: missing host");

        let error = ConfigError::OutOfRange {
            field: "poll-interval",
            constraint: "greater than 0",
            value: "0".to_string(),
        };
        assert_eq!(error.to_string(), "poll-interval must be greater than 0, got 0");

        let error = ConfigError::Metrics("port must be non-zero".to_string());
        assert_eq!(error.to_string(), "invalid metrics config: port must be non-zero");

        let error = ConfigError::Rpc("RPC port must be non-zero".to_string());
        assert_eq!(error.to_string(), "invalid RPC config: RPC port must be non-zero");

        let error = ConfigError::Signing("missing key".to_string());
        assert_eq!(error.to_string(), "invalid signing config: missing key");
    }

    #[test]
    fn test_signing_config_local() {
        let cli = minimal_cli();
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(matches!(config.signing, base_tx_manager::SignerConfig::Local { .. }));
    }

    #[test]
    fn test_signing_config_remote() {
        let mut cli = minimal_cli();
        cli.proposer.signer = SignerCli {
            private_key: None,
            signer_endpoint: Some(Url::parse("http://localhost:8546").unwrap()),
            signer_address: Some("0x1234567890123456789012345678901234567890".parse().unwrap()),
        };
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(matches!(config.signing, base_tx_manager::SignerConfig::Remote { .. }));
    }

    #[test]
    fn test_signing_config_none_provided() {
        let mut cli = minimal_cli();
        cli.proposer.signer =
            SignerCli { private_key: None, signer_endpoint: None, signer_address: None };
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_retry_config_from_args() {
        let cli = minimal_cli();
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert_eq!(config.retry.max_attempts, 5);
        assert_eq!(config.retry.initial_delay, Duration::from_millis(100));
        assert_eq!(config.retry.max_delay, Duration::from_secs(10));
    }
}
