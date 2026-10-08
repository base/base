//! Environment-driven configuration for the challenger E2E driver.
//!
//! The `BASE_CHALLENGER_*` variables are shared with the challenger under test
//! — both containers source the same config-service mapping — so the driver
//! forks exactly the L1 the challenger is configured against. The
//! `CHALLENGER_E2E_*` variables belong to the driver alone.

use std::time::Duration;

use alloy_primitives::Address;
use clap::{Parser, ValueEnum};
use url::Url;

/// E2E scenario to execute.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, ValueEnum)]
pub enum Scenario {
    /// Existing combined Path 1, Path 2 skip, and Path 4 run.
    #[default]
    All,
    /// Path 1 followed by both the skip and dispute halves of Path 2.
    Path1Path2,
    /// Invalid ZK-only proposal coverage.
    Path3,
}

/// Where proofs come from.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, ValueEnum)]
pub enum ProverMode {
    /// An in-pod mock prover plus mock verifiers on the fork. Uses no shared
    /// prover capacity; see `mock_prover` and `mock_verifier`.
    #[default]
    Mock,
    /// The live prover-service at `BASE_CHALLENGER_ZK_RPC_URL` and the fork's
    /// real verifiers. Each `path3`/`all` run costs a 600-block SNARK on the
    /// shared SP1 cluster.
    Real,
}

/// Runtime configuration for [`crate::ChallengerE2e`].
#[derive(Debug, Parser)]
#[command(name = "challenger-e2e", version, about, long_about = None)]
pub struct Config {
    /// E2E scenario to execute.
    #[arg(long, env = "CHALLENGER_E2E_SCENARIO", default_value = "all")]
    pub scenario: Scenario,

    /// L1 RPC that Anvil forks. Only ever read from.
    #[arg(long = "l1-eth-rpc", env = "BASE_CHALLENGER_L1_ETH_RPC")]
    pub l1_eth_rpc: Url,

    /// L2 archive RPC used to compute canonical output roots.
    #[arg(long = "l2-eth-rpc", env = "BASE_CHALLENGER_L2_ETH_RPC")]
    pub l2_eth_rpc: Url,

    /// Where proofs come from. `mock` keeps the run off the shared prover.
    #[arg(long, env = "CHALLENGER_E2E_PROVER", default_value = "mock")]
    pub prover: ProverMode,

    /// How long the mock prover reports a session as running before it
    /// succeeds, so the callers' polling loops are exercised.
    #[arg(
        long = "mock-proving-time",
        env = "CHALLENGER_E2E_MOCK_PROVING_TIME",
        default_value = "5s",
        value_parser = humantime::parse_duration
    )]
    pub mock_proving_time: Duration,

    /// Prover-service JSON-RPC. In `real` mode, the live service the driver and
    /// the challenger both use; in `mock` mode it is replaced at startup by the
    /// in-pod mock's URL. Never the Anvil fork URL.
    #[arg(long = "zk-rpc-url", env = "BASE_CHALLENGER_ZK_RPC_URL")]
    pub zk_rpc_url: Url,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(long = "dispute-game-factory-addr", env = "BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR")]
    pub dispute_game_factory_addr: Address,

    /// Game type ID for `AggregateVerifier` dispute games.
    #[arg(long = "game-type", env = "BASE_CHALLENGER_GAME_TYPE")]
    pub game_type: u32,

    /// `AnchorStateRegistry` on L1.
    ///
    /// Read only to find the anchor game's factory index. The challenger scans
    /// from one past it, so a game at or before the anchor is one it will never
    /// look at.
    #[arg(long = "anchor-state-registry-addr", env = "BASE_CHALLENGER_ANCHOR_STATE_REGISTRY_ADDR")]
    pub anchor_state_registry_addr: Address,

    /// Port Anvil binds to.
    ///
    /// Deliberately not 8545: the production challenger config reserves that
    /// for the keychain signer sidecar.
    #[arg(long = "anvil-port", env = "CHALLENGER_E2E_ANVIL_PORT", default_value = "18545")]
    pub anvil_port: u16,

    /// Handshake file the driver writes to release the challenger. The chart's
    /// sidecar waits on the default; override only to run the pair outside the
    /// pod, e.g. locally.
    #[arg(
        long = "env-file",
        env = "CHALLENGER_E2E_ENV_FILE",
        default_value = "/shared/challenger.env"
    )]
    pub env_file: std::path::PathBuf,

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

    /// Fork block, recorded at startup. Every block after it was mined on the
    /// fork, so it bounds the scan for the challenger's own transactions.
    #[arg(skip)]
    pub fork_block: u64,

    /// Requests the mock prover accepted, in `mock` mode. Set by the driver
    /// when it starts the mock; checked against each disputed checkpoint.
    #[arg(skip)]
    pub mock_requests: Option<crate::mock_prover::MockProofRequests>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_path1_path2_scenario() {
        assert_eq!(Scenario::from_str("path1-path2", false), Ok(Scenario::Path1Path2));
        assert_eq!(Scenario::from_str("path3", false), Ok(Scenario::Path3));
    }

    #[test]
    fn prover_defaults_to_mock() {
        let config = Config::try_parse_from([
            "challenger-e2e",
            "--l1-eth-rpc=http://l1",
            "--l2-eth-rpc=http://l2",
            "--zk-rpc-url=http://zk",
            "--dispute-game-factory-addr=0x0000000000000000000000000000000000000001",
            "--game-type=621",
            "--anchor-state-registry-addr=0x0000000000000000000000000000000000000002",
        ])
        .expect("parses");
        assert_eq!(config.prover, ProverMode::Mock);
        assert_eq!(ProverMode::from_str("real", false), Ok(ProverMode::Real));
    }
}
