//! Environment-driven configuration for the challenger E2E driver.
//!
//! The `BASE_CHALLENGER_*` variables are shared with the challenger under test
//! — both containers source the same config-service mapping — so the driver
//! forks exactly the L1 the challenger is configured against. The
//! `CHALLENGER_E2E_*` variables belong to the driver alone.

use std::{path::PathBuf, time::Duration};

use alloy_primitives::Address;
use clap::Parser;
use url::Url;

/// Runtime configuration for [`crate::ChallengerE2e`].
#[derive(Debug, Parser)]
#[command(name = "challenger-e2e", version, about, long_about = None)]
pub struct Config {
    /// L1 RPC that Anvil forks. Only ever read from.
    #[arg(long = "l1-eth-rpc", env = "BASE_CHALLENGER_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,

    /// L2 archive RPC used to compute canonical output roots.
    #[arg(long = "l2-eth-rpc", env = "BASE_CHALLENGER_L2_ETH_RPC")]
    pub l2_eth_rpc: Url,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(long = "dispute-game-factory-addr", env = "BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR")]
    pub dispute_game_factory_addr: Address,

    /// Game type ID for `AggregateVerifier` dispute games.
    #[arg(long = "game-type", env = "BASE_CHALLENGER_GAME_TYPE")]
    pub game_type: u32,

    /// Port Anvil binds to.
    ///
    /// Deliberately not 8545: the production challenger config reserves that
    /// for the keychain signer sidecar.
    #[arg(long = "anvil-port", env = "CHALLENGER_E2E_ANVIL_PORT", default_value = "18545")]
    pub anvil_port: u16,

    /// File the driver writes to release the challenger sidecar.
    #[arg(
        long = "challenger-env-file",
        env = "CHALLENGER_E2E_CHALLENGER_ENV_FILE",
        default_value = "/shared/challenger.env"
    )]
    pub challenger_env_file: PathBuf,

    /// Prometheus endpoint of the challenger under test.
    #[arg(
        long = "challenger-metrics-url",
        env = "CHALLENGER_E2E_CHALLENGER_METRICS_URL",
        default_value = "http://127.0.0.1:7300/metrics"
    )]
    pub challenger_metrics_url: Url,

    /// How far back through factory indices to look for a game to corrupt.
    #[arg(long = "game-lookback", env = "CHALLENGER_E2E_GAME_LOOKBACK", default_value = "50")]
    pub game_lookback: u64,

    /// Budget for spawning the fork and for the challenger's first scan.
    #[arg(
        long = "startup-timeout",
        env = "CHALLENGER_E2E_STARTUP_TIMEOUT",
        default_value = "5m",
        value_parser = humantime::parse_duration
    )]
    pub startup_timeout: Duration,

    /// How long the fork is left healthy before it is corrupted.
    ///
    /// Must span several challenger poll intervals, otherwise the positive
    /// case proves nothing.
    #[arg(
        long = "quiet-window",
        env = "CHALLENGER_E2E_QUIET_WINDOW",
        default_value = "90s",
        value_parser = humantime::parse_duration
    )]
    pub quiet_window: Duration,

    /// Budget for the challenger to dispute the corrupted game.
    ///
    /// Sized for the ZK fallback path, which waits on a real SNARK proof.
    #[arg(
        long = "dispute-timeout",
        env = "CHALLENGER_E2E_DISPUTE_TIMEOUT",
        default_value = "45m",
        value_parser = humantime::parse_duration
    )]
    pub dispute_timeout: Duration,

    /// Interval between driver polls of the fork and the metrics endpoint.
    #[arg(
        long = "poll-interval",
        env = "CHALLENGER_E2E_POLL_INTERVAL",
        default_value = "5s",
        value_parser = humantime::parse_duration
    )]
    pub poll_interval: Duration,
}
