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
#[derive(Parser)]
#[command(next_help_heading = "Challenger")]
pub struct ChallengerArgs {
    /// URL of the L1 Ethereum RPC endpoint.
    #[arg(long = "l1-eth-rpc", env = "CHALLENGER_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,

    /// URL of the L2 Ethereum RPC endpoint.
    #[arg(long = "l2-eth-rpc", env = "CHALLENGER_L2_ETH_RPC")]
    pub l2_eth_rpc: Url,

    /// URL of the rollup RPC endpoint.
    #[arg(long = "rollup-rpc", env = "CHALLENGER_ROLLUP_RPC")]
    pub rollup_rpc: Url,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(long = "dispute-game-factory-addr", env = "CHALLENGER_DISPUTE_GAME_FACTORY_ADDR")]
    pub dispute_game_factory_addr: Address,

    /// Address of the `AnchorStateRegistry` contract on L1.
    #[arg(long = "anchor-state-registry-addr", env = "CHALLENGER_ANCHOR_STATE_REGISTRY_ADDR")]
    pub anchor_state_registry_addr: Address,

    /// Polling interval for new dispute games (e.g., "12s", "1m").
    #[arg(
        long = "poll-interval",
        env = "CHALLENGER_POLL_INTERVAL",
        default_value = "12s",
        value_parser = humantime::parse_duration
    )]
    pub poll_interval: Duration,

    /// URL of the ZK proof service endpoint.
    #[arg(long = "zk-proof-service-endpoint", env = "CHALLENGER_ZK_PROOF_SERVICE_ENDPOINT")]
    pub zk_proof_service_endpoint: Url,

    /// URL of the signer sidecar JSON-RPC endpoint (for production).
    /// Must be used together with --signer-address.
    #[arg(long = "signer-endpoint", env = "CHALLENGER_SIGNER_ENDPOINT")]
    pub signer_endpoint: Option<Url>,

    /// Address of the signer account on the signer sidecar.
    /// Must be used together with --signer-endpoint.
    #[arg(long = "signer-address", env = "CHALLENGER_SIGNER_ADDRESS")]
    pub signer_address: Option<Address>,

    /// Number of past games to scan on startup.
    #[arg(long = "lookback-games", env = "CHALLENGER_LOOKBACK_GAMES", default_value = "1000")]
    pub lookback_games: u64,

    /// Health server bind address.
    #[arg(long = "health.addr", env = "CHALLENGER_HEALTH_ADDR", default_value = "0.0.0.0")]
    pub health_addr: IpAddr,

    /// URL of the TEE enclave endpoint for nullification proof generation.
    /// When absent, TEE proof generation is disabled (falls back to ZK).
    #[arg(long = "tee-endpoint", env = "CHALLENGER_TEE_ENDPOINT")]
    pub tee_endpoint: Option<Url>,

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
            .field("poll_interval", &self.poll_interval)
            .field("zk_proof_service_endpoint", &self.zk_proof_service_endpoint)
            .field("signer_endpoint", &self.signer_endpoint)
            .field("signer_address", &self.signer_address)
            .field("lookback_games", &self.lookback_games)
            .field("tee_endpoint", &self.tee_endpoint)
            .field("health_addr", &self.health_addr)
            .field("health_port", &self.health_port)
            .finish()
    }
}
