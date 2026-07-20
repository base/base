//! Configuration types and validation for the proposer.

use std::{net::SocketAddr, time::Duration};

use alloy_primitives::{Address, B256};
use base_cli_utils::{LogConfig, MetricsConfig};
use base_retry::RetryConfig;
use eyre::{Result, WrapErr};
use url::Url;

use crate::cli::Cli;

/// Validated proposer configuration.
#[derive(Debug)]
pub struct ProposerConfig {
    /// Dry-run mode: source proofs but do not submit transactions onchain.
    pub dry_run: bool,
    /// URL of the prover RPC endpoint.
    pub prover_rpc: Url,
    /// Prover RPC request timeout.
    pub prover_timeout: Duration,
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
    /// Maximum number of concurrent RPC calls during the recovery scan.
    pub recovery_scan_concurrency: usize,
}

impl ProposerConfig {
    /// Create a validated configuration from CLI arguments.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let Cli { proposer, logging, metrics, health, admin } = cli;

        for (url, message) in [
            (&proposer.prover_rpc, "invalid prover-rpc URL: missing host"),
            (&proposer.l1_eth_rpc, "invalid l1-eth-rpc URL: missing host"),
            (&proposer.l2_eth_rpc, "invalid l2-eth-rpc URL: missing host"),
            (&proposer.rollup_rpc, "invalid rollup-rpc URL: missing host"),
        ] {
            if url.host().is_none() {
                eyre::bail!(message);
            }
        }

        // A zero address would be indistinguishable from an unconfigured value,
        // and is used as the "no parent" sentinel for the first game from anchor state.
        if proposer.anchor_state_registry_addr == Address::ZERO {
            eyre::bail!("anchor-state-registry-addr must be non-zero address");
        }

        if proposer.prover_timeout.is_zero() {
            eyre::bail!("prover-timeout must be greater than 0");
        }

        if proposer.poll_interval.is_zero() {
            eyre::bail!("poll-interval must be greater than 0");
        }

        if metrics.enabled && metrics.port == 0 {
            eyre::bail!("metrics-port must be non-zero when metrics are enabled");
        }

        if health.port == 0 {
            eyre::bail!("health-port must be non-zero");
        }

        if admin.admin_enabled && admin.admin_port == 0 {
            eyre::bail!("admin-port must be non-zero when admin is enabled");
        }

        let (signing, tx_manager) = if proposer.dry_run {
            (None, None)
        } else {
            (
                Some(
                    base_tx_manager::SignerConfig::try_from(proposer.signer)
                        .wrap_err("invalid signing config")?,
                ),
                Some(
                    base_tx_manager::TxManagerConfig::try_from(proposer.tx_manager)
                        .wrap_err("invalid tx manager config")?,
                ),
            )
        };

        Ok(Self {
            dry_run: proposer.dry_run,
            prover_rpc: proposer.prover_rpc,
            prover_timeout: proposer.prover_timeout,
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
            admin_addr: admin
                .admin_enabled
                .then_some(SocketAddr::new(admin.admin_addr, admin.admin_port)),
            retry: RetryConfig::new(
                proposer.rpc_max_retries,
                proposer.rpc_retry_initial_delay,
                proposer.rpc_retry_max_delay,
            ),
            signing,
            tx_manager,
            recovery_scan_concurrency: proposer.recovery_scan_concurrency.get(),
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, SignerCli};

    type InvalidCase = (fn(&mut Cli), &'static str);

    fn minimal_cli() -> Cli {
        Cli::try_parse_from([
            "proposer",
            "--prover-rpc",
            "http://localhost:8080",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--anchor-state-registry-addr",
            "0x1234567890123456789012345678901234567890",
            "--dispute-game-factory-addr",
            "0x2234567890123456789012345678901234567890",
            "--game-type",
            "1",
            "--tee-image-hash",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            "--rollup-rpc",
            "http://localhost:7545",
            "--private-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ])
        .unwrap()
    }

    fn assert_invalid_after(mutate: impl FnOnce(&mut Cli), expected: &'static str) {
        let mut cli = minimal_cli();
        mutate(&mut cli);
        assert_eq!(ProposerConfig::from_cli(cli).unwrap_err().to_string(), expected);
    }

    #[test]
    fn test_valid_config_maps_cli_fields() {
        let mut cli = minimal_cli();
        cli.proposer.rpc_max_retries = 7;
        cli.proposer.recovery_scan_concurrency = 4.try_into().unwrap();
        cli.admin.admin_enabled = true;

        let config = ProposerConfig::from_cli(cli).unwrap();

        assert!(!config.dry_run);
        assert_eq!(config.game_type, 1);
        assert_eq!(config.retry.max_attempts, Some(7));
        assert_eq!(config.recovery_scan_concurrency, 4);
        assert_eq!(config.admin_addr.unwrap().port(), 8545);
        assert!(matches!(config.signing, Some(base_tx_manager::SignerConfig::Local { .. })));
        assert!(config.tx_manager.is_some());
    }

    #[test]
    fn test_invalid_values() {
        let cases: [InvalidCase; 6] = [
            (
                |cli| cli.proposer.prover_timeout = Duration::ZERO,
                "prover-timeout must be greater than 0",
            ),
            (
                |cli| cli.proposer.poll_interval = Duration::ZERO,
                "poll-interval must be greater than 0",
            ),
            (
                |cli| {
                    cli.metrics.enabled = true;
                    cli.metrics.port = 0;
                },
                "metrics-port must be non-zero when metrics are enabled",
            ),
            (|cli| cli.health.port = 0, "health-port must be non-zero"),
            (
                |cli| {
                    cli.admin.admin_enabled = true;
                    cli.admin.admin_port = 0;
                },
                "admin-port must be non-zero when admin is enabled",
            ),
            (
                |cli| cli.proposer.anchor_state_registry_addr = Address::ZERO,
                "anchor-state-registry-addr must be non-zero address",
            ),
        ];

        for (mutate, expected) in cases {
            assert_invalid_after(mutate, expected);
        }
    }

    #[test]
    fn test_disabled_servers_allow_zero_ports() {
        let mut cli = minimal_cli();
        cli.metrics.enabled = false;
        cli.metrics.port = 0;
        cli.admin.admin_enabled = false;
        cli.admin.admin_port = 0;
        assert!(ProposerConfig::from_cli(cli).is_ok());
    }

    #[test]
    fn test_url_without_host() {
        assert_invalid_after(
            |cli| cli.proposer.prover_rpc = Url::parse("file:///some/path").unwrap(),
            "invalid prover-rpc URL: missing host",
        );
    }

    #[test]
    fn test_signing_config_none_provided() {
        let mut cli = minimal_cli();
        cli.proposer.signer =
            SignerCli { private_key: None, signer_endpoint: None, signer_address: None };
        let result = ProposerConfig::from_cli(cli);
        assert_eq!(result.unwrap_err().to_string(), "invalid signing config");
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
}
