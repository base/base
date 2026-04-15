//! CLI argument definitions for proposer.

use std::time::Duration;

use alloy_primitives::{Address, B256};
use base_cli_utils::CliStyles;
use clap::Parser;
use url::Url;

use crate::constants::RECOVERY_SCAN_CONCURRENCY;

base_cli_utils::define_cli_env!("BASE_PROPOSER");
base_cli_utils::define_log_args!("BASE_PROPOSER");
base_cli_utils::define_metrics_args!("BASE_PROPOSER", 7300);
base_cli_utils::define_health_args!("BASE_PROPOSER", 8080);
base_tx_manager::define_signer_cli!("BASE_PROPOSER");
base_tx_manager::define_tx_manager_cli!("BASE_PROPOSER");

/// Proposer - TEE-based output proposal generation for Base.
#[derive(Debug, Clone, Parser)]
#[command(name = "proposer")]
#[command(version, about, long_about = None)]
#[command(styles = CliStyles::init())]
pub struct Cli {
    /// Proposer configuration arguments.
    #[command(flatten)]
    pub proposer: ProposerArgs,

    /// Logging configuration arguments.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration arguments.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// Health server configuration arguments.
    #[command(flatten)]
    pub health: HealthArgs,

    /// Admin RPC configuration arguments.
    #[command(flatten)]
    pub admin: AdminArgs,
}

/// Core proposer configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Proposer")]
pub struct ProposerArgs {
    /// Dry-run mode: source proofs but do not submit transactions on-chain.
    #[arg(long = "dry-run", env = cli_env!("DRY_RUN"), default_value = "false")]
    pub dry_run: bool,

    /// Allow proposals based on non-finalized L1 data.
    #[arg(
        long = "allow-non-finalized",
        env = cli_env!("ALLOW_NON_FINALIZED"),
        default_value = "false"
    )]
    pub allow_non_finalized: bool,

    /// URL of the prover RPC endpoint.
    #[arg(long = "prover-rpc", env = cli_env!("PROVER_RPC"))]
    pub prover_rpc: Url,

    /// URL of the L1 Ethereum RPC endpoint.
    #[arg(long = "l1-eth-rpc", env = cli_env!("L1_ETH_RPC"))]
    pub l1_eth_rpc: Url,

    /// URL of the L2 Ethereum RPC endpoint.
    #[arg(long = "l2-eth-rpc", env = cli_env!("L2_ETH_RPC"))]
    pub l2_eth_rpc: Url,

    /// Address of the `AnchorStateRegistry` contract on L1.
    #[arg(long = "anchor-state-registry-addr", env = cli_env!("ANCHOR_STATE_REGISTRY_ADDR"))]
    pub anchor_state_registry_addr: Address,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(long = "dispute-game-factory-addr", env = cli_env!("DISPUTE_GAME_FACTORY_ADDR"))]
    pub dispute_game_factory_addr: Address,

    /// Game type ID for `AggregateVerifier` dispute games.
    #[arg(long = "game-type", env = cli_env!("GAME_TYPE"))]
    pub game_type: u32,

    /// Keccak256 hash of the TEE image PCR0 (0x-prefixed hex).
    #[arg(long = "tee-image-hash", env = cli_env!("TEE_IMAGE_HASH"))]
    pub tee_image_hash: B256,

    /// Polling interval for new blocks (e.g., "12s", "1m").
    #[arg(
        long = "poll-interval",
        env = cli_env!("POLL_INTERVAL"),
        default_value = "12s",
        value_parser = humantime::parse_duration
    )]
    pub poll_interval: Duration,

    /// RPC request timeout (e.g., "30s", "1m").
    #[arg(
        long = "rpc-timeout",
        env = cli_env!("RPC_TIMEOUT"),
        default_value = "30s",
        value_parser = humantime::parse_duration
    )]
    pub rpc_timeout: Duration,

    /// URL of the rollup RPC endpoint.
    #[arg(long = "rollup-rpc", env = cli_env!("ROLLUP_RPC"))]
    pub rollup_rpc: Url,

    /// Skip TLS certificate verification.
    #[arg(
        long = "skip-tls-verify",
        env = cli_env!("SKIP_TLS_VERIFY"),
        default_value = "false"
    )]
    pub skip_tls_verify: bool,

    /// Maximum number of retry attempts for RPC operations.
    #[arg(long = "rpc-max-retries", env = cli_env!("RPC_MAX_RETRIES"), default_value = "5")]
    pub rpc_max_retries: u32,

    /// Initial delay for exponential backoff (e.g., "100ms", "1s").
    #[arg(
        long = "rpc-retry-initial-delay",
        env = cli_env!("RPC_RETRY_INITIAL_DELAY"),
        default_value = "100ms",
        value_parser = humantime::parse_duration
    )]
    pub rpc_retry_initial_delay: Duration,

    /// Maximum delay between retry attempts (e.g., "10s", "1m").
    #[arg(
        long = "rpc-retry-max-delay",
        env = cli_env!("RPC_RETRY_MAX_DELAY"),
        default_value = "10s",
        value_parser = humantime::parse_duration
    )]
    pub rpc_retry_max_delay: Duration,

    /// Signer configuration (local key or remote sidecar).
    #[command(flatten)]
    pub signer: SignerCli,

    /// Maximum number of concurrent proof tasks in parallel pipeline mode.
    /// Set to 1 for sequential proving (default driver behavior).
    #[arg(
        long = "max-parallel-proofs",
        env = cli_env!("MAX_PARALLEL_PROOFS"),
        default_value = "1"
    )]
    pub max_parallel_proofs: usize,

    /// Maximum number of concurrent RPC calls during the recovery scan.
    #[arg(
        long = "recovery-scan-concurrency",
        env = cli_env!("RECOVERY_SCAN_CONCURRENCY"),
        default_value_t = RECOVERY_SCAN_CONCURRENCY
    )]
    pub recovery_scan_concurrency: usize,

    /// Address of the `TEEProverRegistry` contract on L1.
    /// When set, the proposer validates signers before on-chain submission.
    #[arg(
        long = "tee-prover-registry-address",
        env = cli_env!("TEE_PROVER_REGISTRY_ADDRESS")
    )]
    pub tee_prover_registry_address: Option<Address>,

    /// Transaction manager configuration.
    #[command(flatten)]
    pub tx_manager: TxManagerCli,
}

/// Admin RPC server configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Admin RPC")]
pub struct AdminArgs {
    /// Enable the admin RPC server.
    #[arg(
        id = "rpc_enable_admin",
        long = "rpc.enable-admin",
        env = cli_env!("RPC_ENABLE_ADMIN"),
        default_value = "false"
    )]
    pub enabled: bool,

    /// Admin RPC server bind address.
    #[arg(
        id = "rpc_addr",
        long = "rpc.addr",
        default_value = "127.0.0.1",
        env = cli_env!("RPC_ADDR")
    )]
    pub addr: std::net::IpAddr,

    /// Admin RPC server port.
    #[arg(
        id = "rpc_port",
        long = "rpc.port",
        default_value = "8545",
        env = cli_env!("RPC_PORT")
    )]
    pub port: u16,
}

impl Default for AdminArgs {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 8545,
        }
    }
}

impl AdminArgs {
    /// Returns the configured socket address.
    pub const fn socket_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.addr, self.port)
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use base_cli_utils::LogFormat;

    use super::*;

    #[test]
    fn test_cli_defaults() {
        // Test that we can construct minimal CLI args (requires all required fields)
        let args = vec![
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
        ];
        let cli = Cli::try_parse_from(args).unwrap();

        // Check defaults
        assert!(!cli.proposer.dry_run);
        assert!(!cli.proposer.allow_non_finalized);
        assert_eq!(cli.proposer.poll_interval, Duration::from_secs(12));
        assert_eq!(cli.proposer.rpc_timeout, Duration::from_secs(30));
        assert_eq!(cli.proposer.rollup_rpc.as_str(), "http://localhost:7545/");
        assert!(!cli.proposer.skip_tls_verify);
        assert_eq!(cli.proposer.game_type, 1);
        assert_eq!(cli.proposer.max_parallel_proofs, 1);

        assert_eq!(cli.logging.level, 3);
        assert_eq!(cli.logging.stdout_format, LogFormat::Full);
        assert!(!cli.logging.stdout_quiet);

        assert!(!cli.metrics.enabled);
        assert_eq!(cli.metrics.addr, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(cli.metrics.port, 7300);

        assert!(!cli.admin.enabled);
        assert_eq!(cli.admin.addr, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(cli.admin.port, 8545);
        assert_eq!(cli.health.addr, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(cli.health.port, 8080);

        // Check retry defaults
        assert_eq!(cli.proposer.rpc_max_retries, 5);
        assert_eq!(cli.proposer.rpc_retry_initial_delay, Duration::from_millis(100));
        assert_eq!(cli.proposer.rpc_retry_max_delay, Duration::from_secs(10));

        // Check signing defaults (all None)
        assert!(cli.proposer.signer.private_key.is_none());
        assert!(cli.proposer.signer.signer_endpoint.is_none());
        assert!(cli.proposer.signer.signer_address.is_none());
    }

    #[test]
    fn test_cli_missing_required() {
        // Test that missing required fields cause an error
        let args = vec!["proposer"];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn test_cli_missing_rollup_rpc() {
        let args = vec![
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
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }
}
