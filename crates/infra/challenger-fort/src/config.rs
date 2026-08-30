//! Environment-driven configuration for the challenger FORT observer.
//!
//! The `BASE_CHALLENGER_*` variables are shared with the live Challenger —
//! both source the same config-service mapping — so the observer looks at
//! exactly the L1 and factory the Challenger is configured against. The
//! `CHALLENGER_FORT_*` variables belong to the observer alone.

use std::time::Duration;

use alloy_primitives::Address;
use clap::Parser;
use url::Url;

/// Runtime configuration for [`crate::ChallengerFort`].
#[derive(Debug, Parser)]
#[command(name = "challenger-fort", version, about, long_about = None)]
pub struct Config {
    /// Live L1 RPC. Only ever read from.
    #[arg(long = "l1-eth-rpc", env = "BASE_CHALLENGER_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,

    /// L2 RPC used to compute canonical output roots.
    #[arg(long = "l2-eth-rpc", env = "BASE_CHALLENGER_L2_ETH_RPC")]
    pub l2_eth_rpc: Url,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(long = "dispute-game-factory-addr", env = "BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR")]
    pub dispute_game_factory_addr: Address,

    /// Game type ID for `AggregateVerifier` dispute games.
    #[arg(long = "game-type", env = "BASE_CHALLENGER_GAME_TYPE")]
    pub game_type: u32,

    /// Prometheus endpoint of the live Challenger.
    #[arg(
        long = "challenger-metrics-url",
        env = "CHALLENGER_FORT_CHALLENGER_METRICS_URL",
        default_value = "http://base-challenger:7300/metrics"
    )]
    pub challenger_metrics_url: Url,

    /// How far back through factory indices to look for in-progress games.
    #[arg(long = "game-lookback", env = "CHALLENGER_FORT_GAME_LOOKBACK", default_value = "50")]
    pub game_lookback: u64,

    /// Observation budget. FORT polls until this elapses or the pass
    /// conditions hold.
    #[arg(
        long = "window",
        env = "CHALLENGER_FORT_WINDOW",
        default_value = "10m",
        value_parser = humantime::parse_duration
    )]
    pub window: Duration,

    /// Interval between observer polls of L1 and the metrics endpoint.
    #[arg(
        long = "poll-interval",
        env = "CHALLENGER_FORT_POLL_INTERVAL",
        default_value = "5s",
        value_parser = humantime::parse_duration
    )]
    pub poll_interval: Duration,
}
