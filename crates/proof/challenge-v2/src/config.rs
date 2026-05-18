//! Validated runtime configuration for challenger v2.

use std::{net::SocketAddr, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::{LogConfig, MetricsConfig};
use thiserror::Error;
use url::Url;

use crate::cli::Cli;

/// Failure raised by [`ChallengerConfig::from_cli`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// URL parsed but has no host (e.g., `file:///path`).
    #[error("invalid {field} URL: missing host")]
    InvalidUrl {
        /// Field name as seen on the CLI.
        field: &'static str,
    },
    /// Numeric or address field outside its allowed range.
    #[error("{field} must be {constraint}, got {value}")]
    OutOfRange {
        /// Field name as seen on the CLI.
        field: &'static str,
        /// Human-readable description of the accepted range.
        constraint: &'static str,
        /// Offending value as rendered for the error message.
        value: &'static str,
    },
    /// Signer CLI fields are inconsistent or missing.
    #[error("invalid signing config: {0}")]
    Signer(base_tx_manager::ConfigError),
    /// Transaction manager CLI fields are inconsistent.
    #[error("invalid tx manager config: {0}")]
    TxManager(base_tx_manager::ConfigError),
}

/// Validated configuration consumed by `ChallengerService`.
#[derive(Debug)]
pub struct ChallengerConfig {
    /// L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Url,
    /// L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Url,
    /// `DisputeGameFactory` contract address on L1.
    pub dispute_game_factory_addr: Address,
    /// `AnchorStateRegistry` contract address on L1.
    pub anchor_state_registry_addr: Address,
    /// Game type the challenger acts on.
    pub game_type: u32,
    /// Interval between factory scans.
    pub game_poll_interval: Duration,
    /// ZK prover RPC endpoint.
    pub zk_rpc_url: Url,
    /// ZK RPC request timeout.
    pub zk_request_timeout: Duration,
    /// Interval between ZK session polls.
    pub proof_poll_interval: Duration,
    /// Maximum duration of a single ZK proof attempt.
    pub max_proof_duration: Duration,
    /// Maximum ZK proof retries per `(game, index)` pair.
    pub max_proof_retries: u32,
    /// TEE prover RPC endpoint.
    pub tee_rpc_url: Url,
    /// TEE RPC request timeout.
    pub tee_request_timeout: Duration,
    /// Time window scanned each bond discovery tick.
    pub bond_discovery_max_age: Duration,
    /// Interval between bond discovery ticks.
    pub bond_discovery_interval: Duration,
    /// Addresses the challenger claims bonds on behalf of. Empty
    /// disables the bond pipeline.
    pub bond_claim_addresses: Vec<Address>,
    /// Logging subsystem configuration.
    pub log: LogConfig,
    /// Metrics server configuration.
    pub metrics: MetricsConfig,
    /// Health server bind address.
    pub health_addr: SocketAddr,
    /// Signing configuration for L1 transaction submission.
    pub signer_config: base_tx_manager::SignerConfig,
    /// Transaction manager configuration.
    pub tx_manager_config: base_tx_manager::TxManagerConfig,
}

impl ChallengerConfig {
    /// Validates `cli` and returns the runtime configuration.
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        let Cli { challenger, logging, metrics, health } = cli;

        Self::validate_url(&challenger.l1_eth_rpc, "l1-eth-rpc")?;
        Self::validate_url(&challenger.l2_eth_rpc, "l2-eth-rpc")?;
        Self::validate_url(&challenger.zk_rpc_url, "zk-rpc-url")?;
        Self::validate_url(&challenger.tee_rpc_url, "tee-rpc-url")?;

        Self::reject_zero_address(
            challenger.dispute_game_factory_addr,
            "dispute-game-factory-addr",
        )?;
        Self::reject_zero_address(
            challenger.anchor_state_registry_addr,
            "anchor-state-registry-addr",
        )?;

        Self::reject_zero_duration(challenger.game_poll_interval, "game-poll-interval")?;
        Self::reject_zero_duration(challenger.zk_request_timeout, "zk-request-timeout")?;
        Self::reject_zero_duration(challenger.proof_poll_interval, "proof-poll-interval")?;
        Self::reject_zero_duration(challenger.max_proof_duration, "max-proof-duration")?;
        Self::reject_zero_duration(challenger.tee_request_timeout, "tee-request-timeout")?;
        Self::reject_zero_duration(challenger.bond_discovery_max_age, "bond-discovery-max-age")?;
        Self::reject_zero_duration(challenger.bond_discovery_interval, "bond-discovery-interval")?;

        if health.port == 0 {
            return Err(ConfigError::OutOfRange {
                field: "health-port",
                constraint: "non-zero",
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

        let signer_config = base_tx_manager::SignerConfig::try_from(challenger.signer)
            .map_err(ConfigError::Signer)?;
        let tx_manager_config = base_tx_manager::TxManagerConfig::try_from(challenger.tx_manager)
            .map_err(ConfigError::TxManager)?;

        Ok(Self {
            l1_eth_rpc: challenger.l1_eth_rpc,
            l2_eth_rpc: challenger.l2_eth_rpc,
            dispute_game_factory_addr: challenger.dispute_game_factory_addr,
            anchor_state_registry_addr: challenger.anchor_state_registry_addr,
            game_type: challenger.game_type,
            game_poll_interval: challenger.game_poll_interval,
            zk_rpc_url: challenger.zk_rpc_url,
            zk_request_timeout: challenger.zk_request_timeout,
            proof_poll_interval: challenger.proof_poll_interval,
            max_proof_duration: challenger.max_proof_duration,
            max_proof_retries: challenger.max_proof_retries,
            tee_rpc_url: challenger.tee_rpc_url,
            tee_request_timeout: challenger.tee_request_timeout,
            bond_discovery_max_age: challenger.bond_discovery_max_age,
            bond_discovery_interval: challenger.bond_discovery_interval,
            bond_claim_addresses: challenger.bond_claim_addresses,
            log: LogConfig::from(logging),
            metrics: metrics.into(),
            health_addr: health.socket_addr(),
            signer_config,
            tx_manager_config,
        })
    }

    fn validate_url(url: &Url, field: &'static str) -> Result<(), ConfigError> {
        if url.host().is_none() {
            return Err(ConfigError::InvalidUrl { field });
        }
        Ok(())
    }

    fn reject_zero_address(addr: Address, field: &'static str) -> Result<(), ConfigError> {
        if addr == Address::ZERO {
            return Err(ConfigError::OutOfRange {
                field,
                constraint: "non-zero address",
                value: "0x0000000000000000000000000000000000000000",
            });
        }
        Ok(())
    }

    const fn reject_zero_duration(d: Duration, field: &'static str) -> Result<(), ConfigError> {
        if d.is_zero() {
            return Err(ConfigError::OutOfRange {
                field,
                constraint: "greater than 0",
                value: "0",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_cli_utils::LogFormat;

    use super::*;
    use crate::cli::{ChallengerArgs, HealthArgs, LogArgs, MetricsArgs, SignerCli, TxManagerCli};

    const FACTORY_ADDR: &str = "0x1111111111111111111111111111111111111111";
    const ANCHOR_ADDR: &str = "0x2222222222222222222222222222222222222222";
    const PRIVATE_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn minimal_cli() -> Cli {
        Cli {
            challenger: ChallengerArgs {
                l1_eth_rpc: Url::parse("http://localhost:8545").unwrap(),
                l2_eth_rpc: Url::parse("http://localhost:9545").unwrap(),
                dispute_game_factory_addr: FACTORY_ADDR.parse().unwrap(),
                anchor_state_registry_addr: ANCHOR_ADDR.parse().unwrap(),
                game_type: 1,
                game_poll_interval: Duration::from_secs(10 * 60),
                zk_rpc_url: Url::parse("http://localhost:7001").unwrap(),
                zk_request_timeout: Duration::from_secs(30),
                proof_poll_interval: Duration::from_secs(10),
                max_proof_duration: Duration::from_secs(70 * 60),
                max_proof_retries: 3,
                tee_rpc_url: Url::parse("http://localhost:7002").unwrap(),
                tee_request_timeout: Duration::from_secs(60),
                bond_discovery_max_age: Duration::from_secs(30 * 24 * 60 * 60),
                bond_discovery_interval: Duration::from_secs(10 * 60),
                bond_claim_addresses: vec![],
                signer: SignerCli {
                    private_key: Some(PRIVATE_KEY.to_string()),
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
            health: HealthArgs::default(),
        }
    }

    #[test]
    fn valid_config_passes() {
        let cfg = ChallengerConfig::from_cli(minimal_cli()).unwrap();
        assert_eq!(cfg.game_type, 1);
        assert!(cfg.bond_claim_addresses.is_empty());
        assert!(matches!(cfg.signer_config, base_tx_manager::SignerConfig::Local { .. }));
    }

    #[test]
    fn invalid_url_rejected() {
        let mut cli = minimal_cli();
        cli.challenger.l1_eth_rpc = Url::parse("file:///etc/passwd").unwrap();
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::InvalidUrl { field: "l1-eth-rpc" })
        ));
    }

    #[test]
    fn zero_anchor_address_rejected() {
        let mut cli = minimal_cli();
        cli.challenger.anchor_state_registry_addr = Address::ZERO;
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::OutOfRange { field: "anchor-state-registry-addr", .. })
        ));
    }

    #[test]
    fn zero_factory_address_rejected() {
        let mut cli = minimal_cli();
        cli.challenger.dispute_game_factory_addr = Address::ZERO;
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::OutOfRange { field: "dispute-game-factory-addr", .. })
        ));
    }

    #[test]
    fn zero_game_poll_interval_rejected() {
        let mut cli = minimal_cli();
        cli.challenger.game_poll_interval = Duration::ZERO;
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::OutOfRange { field: "game-poll-interval", .. })
        ));
    }

    #[test]
    fn zero_bond_discovery_interval_rejected() {
        let mut cli = minimal_cli();
        cli.challenger.bond_discovery_interval = Duration::ZERO;
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::OutOfRange { field: "bond-discovery-interval", .. })
        ));
    }

    #[test]
    fn health_port_zero_rejected() {
        let mut cli = minimal_cli();
        cli.health.port = 0;
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::OutOfRange { field: "health-port", .. })
        ));
    }

    #[test]
    fn metrics_port_zero_when_enabled_rejected() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = true;
        cli.metrics.port = 0;
        assert!(matches!(
            ChallengerConfig::from_cli(cli),
            Err(ConfigError::OutOfRange { field: "metrics-port", .. })
        ));
    }

    #[test]
    fn metrics_port_zero_when_disabled_ok() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = false;
        cli.metrics.port = 0;
        assert!(ChallengerConfig::from_cli(cli).is_ok());
    }

    #[test]
    fn signer_missing_rejected() {
        let mut cli = minimal_cli();
        cli.challenger.signer =
            SignerCli { private_key: None, signer_endpoint: None, signer_address: None };
        assert!(matches!(ChallengerConfig::from_cli(cli), Err(ConfigError::Signer(_))));
    }

    #[test]
    fn remote_signer_accepted() {
        let mut cli = minimal_cli();
        cli.challenger.signer = SignerCli {
            private_key: None,
            signer_endpoint: Some(Url::parse("http://localhost:8546").unwrap()),
            signer_address: Some(FACTORY_ADDR.parse().unwrap()),
        };
        let cfg = ChallengerConfig::from_cli(cli).unwrap();
        assert!(matches!(cfg.signer_config, base_tx_manager::SignerConfig::Remote { .. }));
    }

    #[test]
    fn bond_claim_addresses_preserved() {
        let mut cli = minimal_cli();
        cli.challenger.bond_claim_addresses =
            vec![FACTORY_ADDR.parse().unwrap(), ANCHOR_ADDR.parse().unwrap()];
        let cfg = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(cfg.bond_claim_addresses.len(), 2);
    }
}
