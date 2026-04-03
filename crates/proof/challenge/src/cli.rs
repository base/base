//! CLI argument definitions for the challenger.
//!
//! All flags use the `BASE_CHALLENGER_` environment-variable prefix
//! (e.g. `BASE_CHALLENGER_L1_ETH_RPC`). The default metrics port is **7300**.

use std::time::Duration;

use alloy_primitives::Address;
use base_cli_utils::CliStyles;
use clap::Parser;
use url::Url;

base_cli_utils::define_cli_env!("BASE_CHALLENGER");
base_cli_utils::define_log_args!("BASE_CHALLENGER");
base_cli_utils::define_metrics_args!("BASE_CHALLENGER", 7300);
base_cli_utils::define_health_args!("BASE_CHALLENGER", 8080);
base_tx_manager::define_signer_cli!("BASE_CHALLENGER");
base_tx_manager::define_tx_manager_cli!("BASE_CHALLENGER");

/// Challenger - ZK-proof dispute game challenger for Base.
#[derive(Parser)]
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

    /// Health server configuration arguments.
    #[command(flatten)]
    pub health: HealthArgs,
}

impl std::fmt::Debug for Cli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cli")
            .field("challenger", &self.challenger)
            .field("logging", &self.logging)
            .field("metrics", &self.metrics)
            .field("health", &self.health)
            .finish()
    }
}

/// Core challenger configuration arguments.
#[derive(Parser)]
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

    /// Polling interval for new dispute games (e.g., "12s", "1m").
    #[arg(
        long = "poll-interval",
        env = cli_env!("POLL_INTERVAL"),
        default_value = "12s",
        value_parser = humantime::parse_duration
    )]
    pub poll_interval: Duration,

    /// URL of the ZK RPC endpoint.
    #[arg(long = "zk-rpc-url", env = cli_env!("ZK_RPC_URL"))]
    pub zk_rpc_url: Url,

    /// Timeout for establishing the initial gRPC connection to the ZK proof
    /// service (e.g., "10s", "1m").
    #[arg(
        long = "zk-connect-timeout",
        env = cli_env!("ZK_CONNECT_TIMEOUT"),
        default_value = "10s",
        value_parser = humantime::parse_duration
    )]
    pub zk_connect_timeout: Duration,

    /// Timeout for individual gRPC requests to the ZK proof service
    /// (e.g., "30s", "1m").
    #[arg(
        long = "zk-request-timeout",
        env = cli_env!("ZK_REQUEST_TIMEOUT"),
        default_value = "30s",
        value_parser = humantime::parse_duration
    )]
    pub zk_request_timeout: Duration,

    /// URL of the TEE enclave RPC endpoint (optional; enables TEE-first proof sourcing).
    #[arg(long = "tee-rpc-url", env = cli_env!("TEE_RPC_URL"))]
    pub tee_rpc_url: Option<Url>,

    /// Timeout for individual TEE proof requests (e.g., "1m", "10m").
    #[arg(
        long = "tee-request-timeout",
        env = cli_env!("TEE_REQUEST_TIMEOUT"),
        default_value = "10m",
        value_parser = humantime::parse_duration
    )]
    pub tee_request_timeout: Duration,

    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    pub signer: SignerCli,

    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    pub tx_manager: TxManagerCli,

    /// Number of past games to scan on startup.
    #[arg(long = "lookback-games", env = cli_env!("LOOKBACK_GAMES"), default_value = "1000")]
    pub lookback_games: u64,

    /// Comma-separated list of addresses to claim bonds on behalf of.
    ///
    /// When set, the challenger will automatically resolve games and claim
    /// bond credits for games where the `bondRecipient` matches any of
    /// these addresses. Requires two `claimCredit()` calls per game with
    /// a `DelayedWETH` delay in between.
    #[arg(
        long = "bond-claim-addresses",
        env = cli_env!("BOND_CLAIM_ADDRESSES"),
        value_delimiter = ','
    )]
    pub bond_claim_addresses: Vec<Address>,
}

impl std::fmt::Debug for ChallengerArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChallengerArgs")
            .field("l1_eth_rpc", &self.l1_eth_rpc)
            .field("l2_eth_rpc", &self.l2_eth_rpc)
            .field("dispute_game_factory_addr", &self.dispute_game_factory_addr)
            .field("poll_interval", &self.poll_interval)
            .field("zk_rpc_url", &self.zk_rpc_url)
            .field("zk_connect_timeout", &self.zk_connect_timeout)
            .field("zk_request_timeout", &self.zk_request_timeout)
            .field("tee_rpc_url", &self.tee_rpc_url)
            .field("tee_request_timeout", &self.tee_request_timeout)
            .field("signer", &self.signer)
            .field("tx_manager", &self.tx_manager)
            .field("lookback_games", &self.lookback_games)
            .field("bond_claim_addresses", &self.bond_claim_addresses)
            .finish()
    }
}
