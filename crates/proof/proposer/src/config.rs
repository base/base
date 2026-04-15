//! Configuration types and validation for the proposer.

use std::{net::SocketAddr, time::Duration};

use alloy_primitives::{Address, B256};
use base_cli_utils::{LogConfig, MetricsConfig};
use base_proof_rpc::RetryConfig;
use thiserror::Error;
use url::Url;

use crate::cli::{Cli, ProposerArgs};

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
    /// Invalid signing configuration.
    #[error("invalid signing config: {0}")]
    Signing(base_tx_manager::ConfigError),
    /// Invalid transaction manager configuration.
    #[error("invalid tx manager config: {0}")]
    TxManager(base_tx_manager::ConfigError),
}

/// Validated proposer configuration.
#[derive(Debug)]
pub struct ProposerConfig {
    /// Dry-run mode: source proofs but do not submit transactions on-chain.
    pub dry_run: bool,
    /// Allow proposals based on non-finalized L1 data.
    pub allow_non_finalized: bool,
    /// URL of the prover RPC endpoint.
    pub prover_rpc: Url,
    /// URL of the L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Url,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Url,
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
    /// Logging configuration.
    pub log: LogConfig,
    /// Metrics server configuration.
    pub metrics: MetricsConfig,
    /// Health server socket address.
    pub health_addr: SocketAddr,
    /// Admin RPC server socket address. `None` when admin is disabled.
    pub admin_addr: Option<SocketAddr>,
    /// RPC retry configuration.
    pub retry: RetryConfig,
    /// Signing configuration for L1 transaction submission.
    /// `None` when running in dry-run mode.
    pub signing: Option<base_tx_manager::SignerConfig>,
    /// Transaction manager configuration.
    /// `None` when running in dry-run mode.
    pub tx_manager: Option<base_tx_manager::TxManagerConfig>,
    /// Maximum number of concurrent proof tasks.
    /// When > 1, uses the parallel proving pipeline instead of the sequential driver.
    pub max_parallel_proofs: usize,
    /// Maximum number of concurrent RPC calls during the recovery scan.
    pub recovery_scan_concurrency: usize,
    /// Optional address of the `TEEProverRegistry` contract on L1.
    /// When set, the proposer validates signers before on-chain submission.
    pub tee_prover_registry_address: Option<Address>,
}

impl ProposerConfig {
    /// Create a validated configuration from CLI arguments.
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        let Cli { proposer, logging, metrics, health, admin } = cli;

        validate_url(&proposer.prover_rpc, "prover-rpc")?;
        validate_url(&proposer.l1_eth_rpc, "l1-eth-rpc")?;
        validate_url(&proposer.l2_eth_rpc, "l2-eth-rpc")?;
        validate_url(&proposer.rollup_rpc, "rollup-rpc")?;

        if proposer.max_parallel_proofs == 0 {
            return Err(ConfigError::OutOfRange {
                field: "max-parallel-proofs",
                constraint: "at least 1",
                value: "0",
            });
        }

        if proposer.recovery_scan_concurrency == 0 {
            return Err(ConfigError::OutOfRange {
                field: "recovery-scan-concurrency",
                constraint: "at least 1",
                value: "0",
            });
        }

        // A zero address would be indistinguishable from an unconfigured value,
        // and is used as the "no parent" sentinel for the first game from anchor state.
        if proposer.anchor_state_registry_addr == Address::ZERO {
            return Err(ConfigError::OutOfRange {
                field: "anchor-state-registry-addr",
                constraint: "non-zero address",
                value: "0x0000000000000000000000000000000000000000",
            });
        }

        if proposer.poll_interval.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "poll-interval",
                constraint: "greater than 0",
                value: "0",
            });
        }

        if metrics.enabled && metrics.port == 0 {
            return Err(ConfigError::OutOfRange {
                field: "metrics-port",
                constraint: "non-zero when metrics are enabled",
                value: "0",
            });
        }

        if health.port == 0 {
            return Err(ConfigError::OutOfRange {
                field: "health-port",
                constraint: "non-zero",
                value: "0",
            });
        }

        if admin.enabled && admin.port == 0 {
            return Err(ConfigError::OutOfRange {
                field: "admin-port",
                constraint: "non-zero when admin is enabled",
                value: "0",
            });
        }

        let retry = RetryConfig::from(&proposer);

        let (signing, tx_manager) = if proposer.dry_run {
            (None, None)
        } else {
            let s = base_tx_manager::SignerConfig::try_from(proposer.signer)
                .map_err(ConfigError::Signing)?;
            let t = base_tx_manager::TxManagerConfig::try_from(proposer.tx_manager)
                .map_err(ConfigError::TxManager)?;
            (Some(s), Some(t))
        };

        Ok(Self {
            dry_run: proposer.dry_run,
            allow_non_finalized: proposer.allow_non_finalized,
            prover_rpc: proposer.prover_rpc,
            l1_eth_rpc: proposer.l1_eth_rpc,
            l2_eth_rpc: proposer.l2_eth_rpc,
            anchor_state_registry_addr: proposer.anchor_state_registry_addr,
            dispute_game_factory_addr: proposer.dispute_game_factory_addr,
            game_type: proposer.game_type,
            tee_image_hash: proposer.tee_image_hash,
            poll_interval: proposer.poll_interval,
            rpc_timeout: proposer.rpc_timeout,
            rollup_rpc: proposer.rollup_rpc,
            skip_tls_verify: proposer.skip_tls_verify,
            log: LogConfig::from(logging),
            metrics: metrics.into(),
            health_addr: health.socket_addr(),
            admin_addr: admin.enabled.then(|| admin.socket_addr()),
            retry,
            signing,
            tx_manager,
            max_parallel_proofs: proposer.max_parallel_proofs,
            recovery_scan_concurrency: proposer.recovery_scan_concurrency,
            tee_prover_registry_address: proposer.tee_prover_registry_address,
        })
    }
}

/// Validate that a URL has a host component.
///
/// Scheme is guaranteed present by `url::Url::parse`, but host can be absent
/// (e.g. `file:///path`).
fn validate_url(url: &Url, field: &'static str) -> Result<(), ConfigError> {
    if url.host().is_none() {
        return Err(ConfigError::InvalidUrl { field });
    }

    Ok(())
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
    use crate::cli::{
        AdminArgs, Cli, HealthArgs, LogArgs, MetricsArgs, ProposerArgs, SignerCli, TxManagerCli,
    };

    fn minimal_cli() -> Cli {
        Cli {
            proposer: ProposerArgs {
                dry_run: false,
                allow_non_finalized: false,
                prover_rpc: Url::parse("http://localhost:8080").unwrap(),
                l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
                l2_eth_rpc: Url::parse("http://localhost:9545").unwrap(),
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
                max_parallel_proofs: 1,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
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
            health: HealthArgs::default(),
            admin: AdminArgs::default(),
        }
    }

    #[test]
    fn test_valid_config() {
        let cli = minimal_cli();
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(!config.dry_run);
        assert!(!config.allow_non_finalized);
        assert_eq!(config.game_type, 1);
        assert_eq!(config.poll_interval, Duration::from_secs(12));
        assert_eq!(config.rpc_timeout, Duration::from_secs(30));
        assert_eq!(config.max_parallel_proofs, 1);
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
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "metrics-port", .. })));
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
    fn test_health_port_zero_rejected() {
        let mut cli = minimal_cli();
        cli.health.port = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "health-port", .. })));
    }

    #[test]
    fn test_admin_port_zero_when_admin_enabled() {
        let mut cli = minimal_cli();
        cli.admin.enabled = true;
        cli.admin.port = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(result, Err(ConfigError::OutOfRange { field: "admin-port", .. })));
    }

    #[test]
    fn test_admin_port_zero_when_admin_disabled() {
        let mut cli = minimal_cli();
        cli.admin.enabled = false;
        cli.admin.port = 0;
        // Should be fine since admin is disabled
        let result = ProposerConfig::from_cli(cli);
        assert!(result.is_ok());
    }

    #[test]
    fn test_admin_addr_some_when_enabled() {
        let mut cli = minimal_cli();
        cli.admin.enabled = true;
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(config.admin_addr.is_some());
        let addr = config.admin_addr.unwrap();
        assert_eq!(addr.ip(), "127.0.0.1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(addr.port(), 8545);
    }

    #[test]
    fn test_admin_addr_none_when_disabled() {
        let mut cli = minimal_cli();
        cli.admin.enabled = false;
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(config.admin_addr.is_none());
    }

    #[test]
    fn test_url_without_host() {
        // Create URL that parses but has no host (file:// URLs for instance)
        let url = Url::parse("file:///some/path").unwrap();
        let result = validate_url(&url, "test-field");
        assert!(matches!(result, Err(ConfigError::InvalidUrl { field: "test-field", .. })));
    }

    #[test]
    fn test_signing_config_local() {
        let cli = minimal_cli();
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(matches!(config.signing, Some(base_tx_manager::SignerConfig::Local { .. })));
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
        assert!(matches!(config.signing, Some(base_tx_manager::SignerConfig::Remote { .. })));
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
    fn test_dry_run_skips_signer_validation() {
        let mut cli = minimal_cli();
        cli.proposer.dry_run = true;
        cli.proposer.signer =
            SignerCli { private_key: None, signer_endpoint: None, signer_address: None };
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert!(config.dry_run);
        assert!(config.signing.is_none());
        assert!(config.tx_manager.is_none());
    }

    #[test]
    fn test_retry_config_from_args() {
        let cli = minimal_cli();
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert_eq!(config.retry.max_attempts, 5);
        assert_eq!(config.retry.initial_delay, Duration::from_millis(100));
        assert_eq!(config.retry.max_delay, Duration::from_secs(10));
    }

    #[test]
    fn test_max_parallel_proofs_zero_rejected() {
        let mut cli = minimal_cli();
        cli.proposer.max_parallel_proofs = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange { field: "max-parallel-proofs", .. })
        ));
    }

    #[test]
    fn test_max_parallel_proofs_custom() {
        let mut cli = minimal_cli();
        cli.proposer.max_parallel_proofs = 8;
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert_eq!(config.max_parallel_proofs, 8);
    }

    #[test]
    fn test_recovery_scan_concurrency_zero_rejected() {
        let mut cli = minimal_cli();
        cli.proposer.recovery_scan_concurrency = 0;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange { field: "recovery-scan-concurrency", .. })
        ));
    }

    #[test]
    fn test_recovery_scan_concurrency_custom() {
        let mut cli = minimal_cli();
        cli.proposer.recovery_scan_concurrency = 4;
        let config = ProposerConfig::from_cli(cli).unwrap();
        assert_eq!(config.recovery_scan_concurrency, 4);
    }

    #[test]
    fn test_anchor_state_registry_zero_rejected() {
        let mut cli = minimal_cli();
        cli.proposer.anchor_state_registry_addr = Address::ZERO;
        let result = ProposerConfig::from_cli(cli);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange { field: "anchor-state-registry-addr", .. })
        ));
    }
}
