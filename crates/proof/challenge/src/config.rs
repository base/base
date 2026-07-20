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
    /// URL of the ZK RPC endpoint.
    pub zk_rpc_url: Url,
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
            (&challenger.zk_rpc_url, "invalid zk-rpc-url URL: missing host"),
        ] {
            ensure!(url.has_host(), message);
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
            health_addr: health.socket_addr(),
            metrics: metrics.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::SignerCli;

    type InvalidConfigCase = (fn(&mut Cli), &'static str);

    fn minimal_cli() -> Cli {
        Cli::try_parse_from([
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
        ])
        .unwrap()
    }

    #[test]
    fn test_valid_config() {
        let mut cli = minimal_cli();
        cli.metrics.port = 0;
        let config = ChallengerConfig::from_cli(cli).unwrap();
        assert_eq!(config.game_type, 1);
        assert_eq!(config.metrics.port, 0);
        assert!(matches!(config.signing, SignerConfig::Local { .. }));
        assert_eq!(config.tx_manager, TxManagerConfig::default());
    }

    #[test]
    fn test_invalid_config_rejected() {
        let cases: [InvalidConfigCase; 10] = [
            (
                |cli| cli.challenger.poll_interval = Duration::ZERO,
                "poll-interval must be greater than 0",
            ),
            (
                |cli| cli.challenger.zk_request_timeout = Duration::ZERO,
                "zk-request-timeout must be greater than 0",
            ),
            (
                |cli| cli.challenger.bond_discovery_lookback_games = 0,
                "bond-discovery-lookback-games must be greater than 0",
            ),
            (
                |cli| cli.challenger.bond_discovery_interval = Duration::ZERO,
                "bond-discovery-interval must be greater than 0",
            ),
            (
                |cli| cli.challenger.max_proof_duration = Duration::ZERO,
                "max-proof-duration must be greater than 0",
            ),
            (|cli| cli.health.port = 0, "health.port must be greater than 0"),
            (
                |cli| cli.challenger.anchor_state_registry_addr = Address::ZERO,
                "anchor-state-registry-addr must be non-zero",
            ),
            (
                |cli| {
                    cli.metrics.enabled = true;
                    cli.metrics.port = 0;
                },
                "metrics.port must be greater than 0 when metrics are enabled",
            ),
            (
                |cli| {
                    cli.challenger.signer = SignerCli {
                        private_key: None,
                        signer_endpoint: None,
                        signer_address: None,
                    };
                },
                "invalid signing config",
            ),
            (
                |cli| cli.challenger.zk_rpc_url = Url::parse("file:///no/host").unwrap(),
                "invalid zk-rpc-url URL: missing host",
            ),
        ];

        for (mutate, expected) in cases {
            let mut cli = minimal_cli();
            mutate(&mut cli);
            assert_eq!(ChallengerConfig::from_cli(cli).unwrap_err().to_string(), expected);
        }
    }
}
