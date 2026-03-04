//! CLI argument definitions for the challenger.
//!
//! All challenger-specific flags use the `CHALLENGER_` environment-variable
//! prefix (e.g. `CHALLENGER_L1_ETH_RPC`). Logging and metrics flags use the
//! `BASE_CHALLENGER_` prefix via [`base_cli_utils`] macros. The default
//! metrics port is **7310** (distinct from the proposer's 7300).

use std::{net::IpAddr, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::CliStyles;
use clap::Parser;
use url::Url;

base_cli_utils::define_log_args!("BASE_CHALLENGER");
base_cli_utils::define_metrics_args!("BASE_CHALLENGER", 7310);

/// Challenger - ZK-proof dispute game challenger for OP Stack chains.
#[derive(Clone, Parser)]
#[command(name = "challenger")]
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
}

impl std::fmt::Debug for Cli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cli")
            .field("challenger", &self.challenger)
            .field("logging", &self.logging)
            .field("metrics", &self.metrics)
            .finish()
    }
}

/// Core challenger configuration arguments.
#[derive(Clone, Parser)]
#[command(next_help_heading = "Challenger")]
pub struct ChallengerArgs {
    /// URL of the L1 Ethereum RPC endpoint.
    #[arg(
        long = "l1-eth-rpc",
        env = "CHALLENGER_L1_ETH_RPC",
        value_parser = parse_url
    )]
    pub l1_eth_rpc: Url,

    /// URL of the L2 Ethereum RPC endpoint.
    #[arg(
        long = "l2-eth-rpc",
        env = "CHALLENGER_L2_ETH_RPC",
        value_parser = parse_url
    )]
    pub l2_eth_rpc: Url,

    /// URL of the rollup RPC endpoint.
    #[arg(
        long = "rollup-rpc",
        env = "CHALLENGER_ROLLUP_RPC",
        value_parser = parse_url
    )]
    pub rollup_rpc: Url,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(
        long = "dispute-game-factory-addr",
        env = "CHALLENGER_DISPUTE_GAME_FACTORY_ADDR",
        value_parser = parse_address
    )]
    pub dispute_game_factory_addr: Address,

    /// Address of the `AnchorStateRegistry` contract on L1.
    #[arg(
        long = "anchor-state-registry-addr",
        env = "CHALLENGER_ANCHOR_STATE_REGISTRY_ADDR",
        value_parser = parse_address
    )]
    pub anchor_state_registry_addr: Address,

    /// Game type ID for dispute games to monitor.
    #[arg(long = "game-type", env = "CHALLENGER_GAME_TYPE")]
    pub game_type: u32,

    /// Polling interval for new dispute games (e.g., "12s", "1m").
    #[arg(
        long = "poll-interval",
        env = "CHALLENGER_POLL_INTERVAL",
        default_value = "12s",
        value_parser = parse_duration
    )]
    pub poll_interval: Duration,

    /// URL of the ZK proof service endpoint.
    #[arg(
        long = "zk-proof-service-endpoint",
        env = "CHALLENGER_ZK_PROOF_SERVICE_ENDPOINT",
        value_parser = parse_url
    )]
    pub zk_proof_service_endpoint: Url,

    /// URL of the signer sidecar JSON-RPC endpoint (for production).
    /// Must be used together with --signer-address.
    #[arg(
        long = "signer-endpoint",
        env = "CHALLENGER_SIGNER_ENDPOINT",
        value_parser = parse_url
    )]
    pub signer_endpoint: Option<Url>,

    /// Address of the signer account on the signer sidecar.
    /// Must be used together with --signer-endpoint.
    #[arg(
        long = "signer-address",
        env = "CHALLENGER_SIGNER_ADDRESS",
        value_parser = parse_address
    )]
    pub signer_address: Option<Address>,

    /// Number of past games to scan on startup.
    #[arg(long = "lookback-games", env = "CHALLENGER_LOOKBACK_GAMES", default_value = "1000")]
    pub lookback_games: u64,

    /// Health server bind address.
    #[arg(long = "health.addr", env = "CHALLENGER_HEALTH_ADDR", default_value = "0.0.0.0")]
    pub health_addr: IpAddr,

    /// Health server port.
    #[arg(long = "health.port", env = "CHALLENGER_HEALTH_PORT", default_value = "8080")]
    pub health_port: u16,
}

impl std::fmt::Debug for ChallengerArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChallengerArgs")
            .field("l1_eth_rpc", &self.l1_eth_rpc)
            .field("l2_eth_rpc", &self.l2_eth_rpc)
            .field("rollup_rpc", &self.rollup_rpc)
            .field("dispute_game_factory_addr", &self.dispute_game_factory_addr)
            .field("anchor_state_registry_addr", &self.anchor_state_registry_addr)
            .field("game_type", &self.game_type)
            .field("poll_interval", &self.poll_interval)
            .field("zk_proof_service_endpoint", &self.zk_proof_service_endpoint)
            .field("signer_endpoint", &self.signer_endpoint)
            .field("signer_address", &self.signer_address)
            .field("lookback_games", &self.lookback_games)
            .field("health_addr", &self.health_addr)
            .field("health_port", &self.health_port)
            .finish()
    }
}

/// Parse a duration string like "12s", "5m", "1h".
fn parse_duration(s: &str) -> Result<Duration, humantime::DurationError> {
    humantime::parse_duration(s)
}

/// Parse a URL string.
fn parse_url(s: &str) -> Result<Url, url::ParseError> {
    Url::parse(s)
}

/// Parse an Ethereum address from hex string.
fn parse_address(s: &str) -> Result<Address, alloy_primitives::hex::FromHexError> {
    s.parse()
}

#[cfg(test)]
mod tests {
    use base_cli_utils::LogFormat;

    use super::*;

    #[test]
    fn test_parse_duration_valid() {
        assert_eq!(parse_duration("12s").unwrap(), Duration::from_secs(12));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn test_parse_url_valid() {
        let url = parse_url("https://example.com").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("example.com"));
    }

    #[test]
    fn test_parse_url_invalid() {
        assert!(parse_url("not-a-url").is_err());
    }

    #[test]
    fn test_parse_address_valid() {
        let addr = parse_address("0x1234567890123456789012345678901234567890").unwrap();
        assert_eq!(addr.to_string(), "0x1234567890123456789012345678901234567890");
    }

    #[test]
    fn test_parse_address_invalid() {
        assert!(parse_address("0xnotanaddress").is_err());
        assert!(parse_address("invalid").is_err());
    }

    #[test]
    fn test_cli_defaults() {
        let args = vec![
            "challenger",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--rollup-rpc",
            "http://localhost:7545",
            "--dispute-game-factory-addr",
            "0x1234567890123456789012345678901234567890",
            "--anchor-state-registry-addr",
            "0x2234567890123456789012345678901234567890",
            "--game-type",
            "1",
            "--zk-proof-service-endpoint",
            "http://localhost:5000",
        ];
        let cli = Cli::try_parse_from(args).unwrap();

        // Check defaults
        assert_eq!(cli.challenger.poll_interval, Duration::from_secs(12));
        assert_eq!(cli.challenger.lookback_games, 1000);
        assert_eq!(cli.challenger.game_type, 1);
        assert_eq!(cli.challenger.rollup_rpc.as_str(), "http://localhost:7545/");

        assert_eq!(cli.logging.level, 3);
        assert_eq!(cli.logging.stdout_format, LogFormat::Full);
        assert!(!cli.logging.stdout_quiet);

        assert!(!cli.metrics.enabled);
        assert_eq!(cli.metrics.addr, "0.0.0.0".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(cli.metrics.port, 7310);

        // Check signing defaults (all None)
        assert!(cli.challenger.signer_endpoint.is_none());
        assert!(cli.challenger.signer_address.is_none());

        // Check health server defaults
        assert_eq!(cli.challenger.health_addr, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(cli.challenger.health_port, 8080);
    }

    #[test]
    fn test_cli_missing_required() {
        let args = vec!["challenger"];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn test_cli_missing_rollup_rpc() {
        let args = vec![
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
            "--zk-proof-service-endpoint",
            "http://localhost:5000",
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }

}
