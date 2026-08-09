//! CLI argument definitions for the ZK fork dispute tool.

use std::time::Duration;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use base_challenger::DisputeIntent;
use base_cli_utils::CliStyles;
use base_prover_service_protocol::ZkBackend;
use clap::{Parser, ValueEnum};
use url::Url;

base_cli_utils::define_cli_env!("BASE_ZK_FORK");
base_cli_utils::define_log_args!("BASE_ZK_FORK");

/// ZK fork dispute — patch/find an invalid game, prove, and dispute on Anvil.
#[derive(Debug, Parser)]
#[command(name = "base-zk-fork-dispute")]
#[command(version, about, long_about = None)]
#[command(styles = CliStyles::init())]
pub struct Cli {
    /// Fork dispute configuration.
    #[command(flatten)]
    pub fork: ForkArgs,

    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,
}

/// Core fork-dispute configuration arguments.
#[derive(Debug, Parser)]
#[command(next_help_heading = "Fork Dispute")]
pub struct ForkArgs {
    /// Anvil (or other) L1 fork RPC.
    #[arg(long = "l1-rpc-url", env = cli_env!("L1_RPC_URL"))]
    pub l1_rpc_url: Url,

    /// L2 archive RPC for canonical output roots (`eth_getProof`).
    #[arg(long = "l2-rpc-url", env = cli_env!("L2_RPC_URL"))]
    pub l2_rpc_url: Url,

    /// Prover-service JSON-RPC endpoint.
    #[arg(
        long = "prover-service-url",
        env = cli_env!("PROVER_SERVICE_URL"),
        default_value = "http://localhost:9000"
    )]
    pub prover_service_url: Url,

    /// Prover-service routing version required by the selected game.
    #[arg(long = "proof-protocol-version", env = cli_env!("PROOF_PROTOCOL_VERSION"))]
    pub proof_protocol_version: u32,

    /// `DisputeGameFactory` address.
    #[arg(long = "dispute-game-factory", env = cli_env!("DISPUTE_GAME_FACTORY"))]
    pub dispute_game_factory: Address,

    /// Specific already-invalid dispute game proxy (skips Anvil patching).
    #[arg(
        long = "game-address",
        env = cli_env!("GAME_ADDRESS"),
        conflicts_with = "game_index"
    )]
    pub game_address: Option<Address>,

    /// Factory game index to select and patch (defaults to newest).
    #[arg(
        long = "game-index",
        env = cli_env!("GAME_INDEX"),
        conflicts_with = "game_address"
    )]
    pub game_index: Option<u64>,

    /// Hex-encoded secp256k1 private key for dispute transactions.
    #[arg(long = "private-key", env = cli_env!("PRIVATE_KEY"))]
    pub private_key: PrivateKeySigner,

    /// Dispute intent (`challenge` or `nullify`).
    ///
    /// When omitted, inferred from game provers: TEE-only → challenge, ZK present → nullify.
    #[arg(long = "dispute-intent", env = cli_env!("DISPUTE_INTENT"))]
    pub intent: Option<DisputeIntentArg>,

    /// ZK proving backend for the prover-service request.
    #[arg(
        long = "zk-backend",
        env = cli_env!("ZK_BACKEND"),
        default_value = "cluster"
    )]
    pub zk_backend: ZkBackendArg,

    /// Optional invalid intermediate index override.
    #[arg(long = "invalid-index", env = cli_env!("INVALID_INDEX"))]
    pub invalid_index: Option<u64>,

    /// Poll interval while waiting for a proof.
    #[arg(
        long = "poll-interval",
        env = cli_env!("POLL_INTERVAL"),
        default_value = "30s",
        value_parser = humantime::parse_duration
    )]
    pub poll_interval: Duration,

    /// Proof poll timeout.
    #[arg(
        long = "poll-timeout",
        env = cli_env!("POLL_TIMEOUT"),
        default_value = "4h",
        value_parser = humantime::parse_duration
    )]
    pub poll_timeout: Duration,
}

/// CLI value for [`DisputeIntent`].
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DisputeIntentArg {
    /// Submit via `challenge()`.
    Challenge,
    /// Submit via `nullify()`.
    Nullify,
}

impl From<DisputeIntentArg> for DisputeIntent {
    fn from(value: DisputeIntentArg) -> Self {
        match value {
            DisputeIntentArg::Challenge => Self::Challenge,
            DisputeIntentArg::Nullify => Self::Nullify,
        }
    }
}

/// CLI value for proving backends that return proof bytes.
///
/// [`ZkBackend::DryRun`] is omitted: it does not produce a proof usable for dispute submission.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ZkBackendArg {
    /// Self-hosted SP1 cluster.
    Cluster,
    /// Succinct SP1 prover network.
    Network,
}

impl From<ZkBackendArg> for ZkBackend {
    fn from(value: ZkBackendArg) -> Self {
        match value {
            ZkBackendArg::Cluster => Self::Cluster,
            ZkBackendArg::Network => Self::Network,
        }
    }
}
