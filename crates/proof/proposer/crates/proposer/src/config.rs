//! Configuration types and validation for the proposer.

use std::{net::IpAddr, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::LogConfig;
use thiserror::Error;
use url::Url;

use crate::cli::{Cli, LogArgs, MetricsArgs, RpcServerArgs};

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
}

/// Validated proposer configuration.
#[derive(Debug, Clone)]
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
    /// Minimum number of blocks between proposals.
    pub min_proposal_interval: u64,
    /// Address of the on-chain verifier contract.
    pub onchain_verifier_addr: Address,
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// URL of the rollup RPC endpoint (optional).
    pub rollup_rpc: Option<Url>,
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
}

impl ProposerConfig {
    /// Create a validated configuration from CLI arguments.
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        // Validate URLs have scheme and host
        validate_url(&cli.proposer.enclave_rpc, "enclave-rpc")?;
        validate_url(&cli.proposer.l1_eth_rpc, "l1-eth-rpc")?;
        validate_url(&cli.proposer.l2_eth_rpc, "l2-eth-rpc")?;

        if let Some(ref rollup_rpc) = cli.proposer.rollup_rpc {
            validate_url(rollup_rpc, "rollup-rpc")?;
        }

        // Validate poll_interval > 0
        if cli.proposer.poll_interval.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "poll-interval",
                constraint: "greater than 0",
                value: "0".to_string(),
            });
        }

        // Validate min_proposal_interval > 0
        if cli.proposer.min_proposal_interval == 0 {
            return Err(ConfigError::OutOfRange {
                field: "min-proposal-interval",
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

        Ok(Self {
            allow_non_finalized: cli.proposer.allow_non_finalized,
            enclave_rpc: cli.proposer.enclave_rpc,
            l1_eth_rpc: cli.proposer.l1_eth_rpc,
            l2_eth_rpc: cli.proposer.l2_eth_rpc,
            l2_reth: cli.proposer.l2_reth,
            min_proposal_interval: cli.proposer.min_proposal_interval,
            onchain_verifier_addr: cli.proposer.onchain_verifier_addr,
            poll_interval: cli.proposer.poll_interval,
            rollup_rpc: cli.proposer.rollup_rpc,
            skip_tls_verify: cli.proposer.skip_tls_verify,
            wait_node_sync: cli.proposer.wait_node_sync,
            log: LogConfig::from(cli.logging),
            metrics: MetricsConfig::from(cli.metrics),
            rpc: RpcServerConfig::from(cli.rpc),
        })
    }
}

/// Validate that a URL has a scheme and host.
fn validate_url(url: &Url, field: &'static str) -> Result<(), ConfigError> {
    if url.scheme().is_empty() {
        return Err(ConfigError::InvalidUrl {
            field,
            reason: "missing scheme".to_string(),
        });
    }

    if url.host().is_none() {
        return Err(ConfigError::InvalidUrl {
            field,
            reason: "missing host".to_string(),
        });
    }

    Ok(())
}

impl From<LogArgs> for LogConfig {
    fn from(args: LogArgs) -> Self {
        use base_cli_utils::{StdoutLogConfig, verbosity_to_level_filter};

        let stdout_logs = if args.stdout_quiet {
            None
        } else {
            Some(StdoutLogConfig {
                format: args.stdout_format,
            })
        };

        Self {
            global_level: verbosity_to_level_filter(args.level),
            stdout_logs,
            file_logs: None,
        }
    }
}

/// Validated metrics server configuration.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    /// Whether metrics are enabled.
    pub enabled: bool,
    /// Metrics server bind address.
    pub addr: IpAddr,
    /// Metrics server port.
    pub port: u16,
}

impl From<MetricsArgs> for MetricsConfig {
    fn from(args: MetricsArgs) -> Self {
        Self {
            enabled: args.enabled,
            addr: args.addr,
            port: args.port,
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: "0.0.0.0".parse().unwrap(),
            port: 7300,
        }
    }
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
        Self {
            enable_admin: args.enable_admin,
            addr: args.addr,
            port: args.port,
        }
    }
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self {
            enable_admin: false,
            addr: "127.0.0.1".parse().unwrap(),
            port: 8545,
        }
    }
}

#[cfg(test)]
mod tests {
    use base_cli_utils::LogFormat;

    use crate::cli::{Cli, ProposerArgs};

    use super::*;

    fn minimal_cli() -> Cli {
        Cli {
            proposer: ProposerArgs {
                allow_non_finalized: false,
                enclave_rpc: Url::parse("http://localhost:8080").unwrap(),
                l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
                l2_eth_rpc: Url::parse("http://localhost:9545").unwrap(),
                l2_reth: false,
                min_proposal_interval: 512,
                onchain_verifier_addr: "0x1234567890123456789012345678901234567890"
                    .parse()
                    .unwrap(),
                poll_interval: Duration::from_secs(12),
                rollup_rpc: None,
                skip_tls_verify: false,
                wait_node_sync: false,
            },
            logging: LogArgs {
                level: 3,
                stdout_quiet: false,
                stdout_format: LogFormat::Full,
            },
            metrics: MetricsArgs {
                enabled: false,
                addr: "0.0.0.0".parse().unwrap(),
                port: 7300,
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
        assert_eq!(config.min_proposal_interval, 512);
        assert_eq!(config.poll_interval, Duration::from_secs(12));
    }

    #[test]
    fn test_zero_poll_interval() {
        let mut cli = minimal_cli();
        cli.proposer.poll_interval = Duration::ZERO;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange {
                field: "poll-interval",
                ..
            })
        ));
    }

    #[test]
    fn test_zero_min_proposal_interval() {
        let mut cli = minimal_cli();
        cli.proposer.min_proposal_interval = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange {
                field: "min-proposal-interval",
                ..
            })
        ));
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
    fn test_optional_rollup_rpc() {
        let mut cli = minimal_cli();
        cli.proposer.rollup_rpc = Some(Url::parse("http://localhost:7545").unwrap());
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(config.rollup_rpc.is_some());
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
        };
        let config = MetricsConfig::from(args);
        assert!(config.enabled);
        assert_eq!(config.port, 9090);
    }

    #[test]
    fn test_rpc_server_config_from_args() {
        let args = RpcServerArgs {
            enable_admin: true,
            addr: "0.0.0.0".parse().unwrap(),
            port: 8080,
        };
        let config = RpcServerConfig::from(args);
        assert!(config.enable_admin);
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_url_without_host() {
        // Create URL that parses but has no host (file:// URLs for instance)
        let url = Url::parse("file:///some/path").unwrap();
        let result = validate_url(&url, "test-field");
        assert!(matches!(
            result,
            Err(ConfigError::InvalidUrl {
                field: "test-field",
                ..
            })
        ));
    }

    #[test]
    fn test_config_error_display() {
        let error = ConfigError::InvalidUrl {
            field: "enclave-rpc",
            reason: "missing host".to_string(),
        };
        assert_eq!(error.to_string(), "invalid enclave-rpc URL: missing host");

        let error = ConfigError::OutOfRange {
            field: "poll-interval",
            constraint: "greater than 0",
            value: "0".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "poll-interval must be greater than 0, got 0"
        );

        let error = ConfigError::Metrics("port must be non-zero".to_string());
        assert_eq!(
            error.to_string(),
            "invalid metrics config: port must be non-zero"
        );

        let error = ConfigError::Rpc("RPC port must be non-zero".to_string());
        assert_eq!(
            error.to_string(),
            "invalid RPC config: RPC port must be non-zero"
        );
    }
}
