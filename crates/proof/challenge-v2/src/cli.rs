//! CLI argument definitions for challenger v2.

use std::time::Duration;

use alloy_primitives::Address;
use base_cli_utils::CliStyles;
use clap::Parser;
use url::Url;

base_cli_utils::define_cli_env!("BASE_CHALLENGER_V2");
base_cli_utils::define_log_args!("BASE_CHALLENGER_V2");
base_cli_utils::define_metrics_args!("BASE_CHALLENGER_V2", 7300);
base_cli_utils::define_health_args!("BASE_CHALLENGER_V2", 8080);
base_tx_manager::define_signer_cli!("BASE_CHALLENGER_V2");
base_tx_manager::define_tx_manager_cli!("BASE_CHALLENGER_V2", tx_send_timeout_default = "10m");

/// Challenger v2 — per-game async challenger for `AggregateVerifier` dispute games.
#[derive(Debug, Clone, Parser)]
#[command(name = "challenger-v2")]
#[command(version, about, long_about = None)]
#[command(styles = CliStyles::init())]
pub struct Cli {
    /// Challenger configuration arguments.
    #[command(flatten)]
    pub challenger: ChallengerArgs,

    /// Logging configuration arguments.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration arguments.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// Health server configuration arguments.
    #[command(flatten)]
    pub health: HealthArgs,
}

/// Core challenger configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Challenger")]
pub struct ChallengerArgs {
    /// URL of the L1 Ethereum RPC endpoint.
    #[arg(long = "l1-eth-rpc", env = cli_env!("L1_ETH_RPC"))]
    pub l1_eth_rpc: Url,

    /// URL of the L2 Ethereum RPC endpoint.
    #[arg(long = "l2-eth-rpc", env = cli_env!("L2_ETH_RPC"))]
    pub l2_eth_rpc: Url,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(long = "dispute-game-factory-addr", env = cli_env!("DISPUTE_GAME_FACTORY_ADDR"))]
    pub dispute_game_factory_addr: Address,

    /// Address of the `AnchorStateRegistry` contract on L1.
    #[arg(long = "anchor-state-registry-addr", env = cli_env!("ANCHOR_STATE_REGISTRY_ADDR"))]
    pub anchor_state_registry_addr: Address,

    /// Game type ID for `AggregateVerifier` dispute games.
    #[arg(long = "game-type", env = cli_env!("GAME_TYPE"))]
    pub game_type: u32,

    /// Interval between scans of the dispute game factory (e.g., "10m").
    #[arg(
        long = "game-poll-interval",
        env = cli_env!("GAME_POLL_INTERVAL"),
        default_value = "10m",
        value_parser = humantime::parse_duration,
    )]
    pub game_poll_interval: Duration,

    /// URL of the ZK prover RPC endpoint.
    #[arg(long = "zk-rpc-url", env = cli_env!("ZK_RPC_URL"))]
    pub zk_rpc_url: Url,

    /// ZK RPC request timeout (e.g., "30s").
    #[arg(
        long = "zk-request-timeout",
        env = cli_env!("ZK_REQUEST_TIMEOUT"),
        default_value = "30s",
        value_parser = humantime::parse_duration,
    )]
    pub zk_request_timeout: Duration,

    /// Interval between ZK session polls (e.g., "10s").
    #[arg(
        long = "proof-poll-interval",
        env = cli_env!("PROOF_POLL_INTERVAL"),
        default_value = "10s",
        value_parser = humantime::parse_duration,
    )]
    pub proof_poll_interval: Duration,

    /// Maximum total duration for a single ZK proof attempt before
    /// timing out and retrying (e.g., "70m").
    #[arg(
        long = "max-proof-duration",
        env = cli_env!("MAX_PROOF_DURATION"),
        default_value = "70m",
        value_parser = humantime::parse_duration,
    )]
    pub max_proof_duration: Duration,

    /// Maximum number of ZK proof retry attempts for a single
    /// `(game, index)` pair before giving up.
    #[arg(long = "max-proof-retries", env = cli_env!("MAX_PROOF_RETRIES"), default_value = "3")]
    pub max_proof_retries: u32,

    /// URL of the TEE prover RPC endpoint.
    #[arg(long = "tee-rpc-url", env = cli_env!("TEE_RPC_URL"))]
    pub tee_rpc_url: Url,

    /// TEE RPC request timeout (e.g., "60s").
    #[arg(
        long = "tee-request-timeout",
        env = cli_env!("TEE_REQUEST_TIMEOUT"),
        default_value = "60s",
        value_parser = humantime::parse_duration,
    )]
    pub tee_request_timeout: Duration,

    /// Time window scanned each bond-discovery tick, relative to now.
    /// Bounds the per-tick work after a restart (e.g., "30d").
    #[arg(
        long = "bond-discovery-max-age",
        env = cli_env!("BOND_DISCOVERY_MAX_AGE"),
        default_value = "30d",
        value_parser = humantime::parse_duration,
    )]
    pub bond_discovery_max_age: Duration,

    /// Interval between bond discovery scans (e.g., "10m").
    #[arg(
        long = "bond-discovery-interval",
        env = cli_env!("BOND_DISCOVERY_INTERVAL"),
        default_value = "10m",
        value_parser = humantime::parse_duration,
    )]
    pub bond_discovery_interval: Duration,

    /// Comma-separated list of addresses to claim bonds on behalf of.
    /// An empty list disables the bond pipeline.
    #[arg(
        long = "bond-claim-addresses",
        env = cli_env!("BOND_CLAIM_ADDRESSES"),
        value_delimiter = ','
    )]
    pub bond_claim_addresses: Vec<Address>,

    /// Signer configuration (local key or remote sidecar).
    #[command(flatten)]
    pub signer: SignerCli,

    /// Transaction manager configuration.
    #[command(flatten)]
    pub tx_manager: TxManagerCli,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_args() -> Vec<&'static str> {
        vec![
            "challenger-v2",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--dispute-game-factory-addr",
            "0x1111111111111111111111111111111111111111",
            "--anchor-state-registry-addr",
            "0x2222222222222222222222222222222222222222",
            "--game-type",
            "1",
            "--zk-rpc-url",
            "http://localhost:7001",
            "--tee-rpc-url",
            "http://localhost:7002",
            "--private-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ]
    }

    #[test]
    fn parses_minimal_required_args() {
        let cli = Cli::try_parse_from(required_args()).expect("parse");

        assert_eq!(cli.challenger.game_type, 1);
        assert_eq!(cli.challenger.game_poll_interval, Duration::from_secs(10 * 60));
        assert_eq!(cli.challenger.zk_request_timeout, Duration::from_secs(30));
        assert_eq!(cli.challenger.proof_poll_interval, Duration::from_secs(10));
        assert_eq!(cli.challenger.max_proof_duration, Duration::from_secs(70 * 60));
        assert_eq!(cli.challenger.max_proof_retries, 3);
        assert_eq!(cli.challenger.tee_request_timeout, Duration::from_secs(60));
        assert_eq!(cli.challenger.bond_discovery_max_age, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(cli.challenger.bond_discovery_interval, Duration::from_secs(10 * 60));
        assert!(cli.challenger.bond_claim_addresses.is_empty());

        assert!(!cli.metrics.enabled);
        assert_eq!(cli.metrics.port, 7300);
        assert_eq!(cli.health.port, 8080);
    }

    #[test]
    fn missing_required_field_errors() {
        let args = vec!["challenger-v2"];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn bond_claim_addresses_parses_csv() {
        let mut args = required_args();
        args.extend([
            "--bond-claim-addresses",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa,0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ]);
        let cli = Cli::try_parse_from(args).expect("parse");
        assert_eq!(cli.challenger.bond_claim_addresses.len(), 2);
    }
}
