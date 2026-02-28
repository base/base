//! CLI definition for the proposer binary.

use std::{net::IpAddr, time::Duration};

use alloy_primitives::{Address, B256};
use base_cli_utils::CliStyles;
use base_proposer::{
    ConfigError, ProposerConfig, RetryConfig, RpcServerConfig, build_signing_config, validate_url,
};
use clap::Parser;
use url::Url;

base_cli_utils::define_log_args!("BASE_PROPOSER");
base_cli_utils::define_metrics_args!("BASE_PROPOSER", 7300);

/// Proposer - TEE-based output proposal generation for OP Stack chains.
#[derive(Debug, Clone, Parser)]
#[command(name = "proposer")]
#[command(version, about, long_about = None)]
#[command(styles = CliStyles::init())]
pub(crate) struct Cli {
    /// Proposer configuration arguments.
    #[command(flatten)]
    pub proposer: ProposerArgs,

    /// Logging configuration arguments.
    #[command(flatten)]
    pub logging: LogArgs,

    /// Metrics configuration arguments.
    #[command(flatten)]
    pub metrics: MetricsArgs,

    /// RPC server configuration arguments.
    #[command(flatten)]
    pub rpc: RpcServerArgs,
}

/// Core proposer configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Proposer")]
pub(crate) struct ProposerArgs {
    /// Allow proposals based on non-finalized L1 data.
    #[arg(
        long = "allow-non-finalized",
        env = "BASE_PROPOSER_ALLOW_NON_FINALIZED",
        default_value = "false"
    )]
    pub allow_non_finalized: bool,

    /// URL of the enclave RPC endpoint.
    #[arg(
        long = "enclave-rpc",
        env = "BASE_PROPOSER_ENCLAVE_RPC",
        value_parser = parse_url
    )]
    pub enclave_rpc: Url,

    /// URL of the L1 Ethereum RPC endpoint.
    #[arg(
        long = "l1-eth-rpc",
        env = "BASE_PROPOSER_L1_ETH_RPC",
        value_parser = parse_url
    )]
    pub l1_eth_rpc: Url,

    /// URL of the L2 Ethereum RPC endpoint.
    #[arg(
        long = "l2-eth-rpc",
        env = "BASE_PROPOSER_L2_ETH_RPC",
        value_parser = parse_url
    )]
    pub l2_eth_rpc: Url,

    /// Use reth-specific RPC calls for L2.
    #[arg(long = "l2-reth", env = "BASE_PROPOSER_L2_RETH", default_value = "false")]
    pub l2_reth: bool,

    /// Address of the `AnchorStateRegistry` contract on L1.
    #[arg(
        long = "anchor-state-registry-addr",
        env = "BASE_PROPOSER_ANCHOR_STATE_REGISTRY_ADDR",
        value_parser = parse_address
    )]
    pub anchor_state_registry_addr: Address,

    /// Address of the `DisputeGameFactory` contract on L1.
    #[arg(
        long = "dispute-game-factory-addr",
        env = "BASE_PROPOSER_DISPUTE_GAME_FACTORY_ADDR",
        value_parser = parse_address
    )]
    pub dispute_game_factory_addr: Address,

    /// Game type ID for `AggregateVerifier` dispute games.
    #[arg(long = "game-type", env = "BASE_PROPOSER_GAME_TYPE")]
    pub game_type: u32,

    /// Keccak256 hash of the TEE image PCR0 (0x-prefixed hex).
    #[arg(
        long = "tee-image-hash",
        env = "BASE_PROPOSER_TEE_IMAGE_HASH",
        value_parser = parse_b256
    )]
    pub tee_image_hash: B256,

    /// Polling interval for new blocks (e.g., "12s", "1m").
    #[arg(
        long = "poll-interval",
        env = "BASE_PROPOSER_POLL_INTERVAL",
        default_value = "12s",
        value_parser = parse_duration
    )]
    pub poll_interval: Duration,

    /// RPC request timeout (e.g., "30s", "1m").
    #[arg(
        long = "rpc-timeout",
        env = "BASE_PROPOSER_RPC_TIMEOUT",
        default_value = "30s",
        value_parser = parse_duration
    )]
    pub rpc_timeout: Duration,

    /// URL of the rollup RPC endpoint.
    #[arg(
        long = "rollup-rpc",
        env = "BASE_PROPOSER_ROLLUP_RPC",
        value_parser = parse_url
    )]
    pub rollup_rpc: Url,

    /// Skip TLS certificate verification.
    #[arg(
        long = "skip-tls-verify",
        env = "BASE_PROPOSER_SKIP_TLS_VERIFY",
        default_value = "false"
    )]
    pub skip_tls_verify: bool,

    /// Wait for node sync before starting.
    #[arg(long = "wait-node-sync", env = "BASE_PROPOSER_WAIT_NODE_SYNC", default_value = "false")]
    pub wait_node_sync: bool,

    /// Maximum number of retry attempts for RPC operations.
    #[arg(long = "rpc-max-retries", env = "BASE_PROPOSER_RPC_MAX_RETRIES", default_value = "5")]
    pub rpc_max_retries: u32,

    /// Initial delay for exponential backoff (e.g., "100ms", "1s").
    #[arg(
        long = "rpc-retry-initial-delay",
        env = "BASE_PROPOSER_RPC_RETRY_INITIAL_DELAY",
        default_value = "100ms",
        value_parser = parse_duration
    )]
    pub rpc_retry_initial_delay: Duration,

    /// Maximum delay between retry attempts (e.g., "10s", "1m").
    #[arg(
        long = "rpc-retry-max-delay",
        env = "BASE_PROPOSER_RPC_RETRY_MAX_DELAY",
        default_value = "10s",
        value_parser = parse_duration
    )]
    pub rpc_retry_max_delay: Duration,

    /// Private key for local transaction signing (hex-encoded, for development).
    /// Mutually exclusive with --signer-endpoint/--signer-address.
    #[arg(long = "private-key", env = "BASE_PROPOSER_PRIVATE_KEY")]
    pub private_key: Option<String>,

    /// URL of the signer sidecar JSON-RPC endpoint (for production).
    /// Must be used together with --signer-address.
    #[arg(
        long = "signer-endpoint",
        env = "BASE_PROPOSER_SIGNER_ENDPOINT",
        value_parser = parse_url
    )]
    pub signer_endpoint: Option<Url>,

    /// Address of the signer account on the signer sidecar.
    /// Must be used together with --signer-endpoint.
    #[arg(
        long = "signer-address",
        env = "BASE_PROPOSER_SIGNER_ADDRESS",
        value_parser = parse_address
    )]
    pub signer_address: Option<Address>,
}

/// RPC server configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "RPC Server")]
pub(crate) struct RpcServerArgs {
    /// Enable admin RPC methods.
    #[arg(
        id = "rpc_enable_admin",
        long = "rpc.enable-admin",
        env = "BASE_PROPOSER_RPC_ENABLE_ADMIN",
        default_value = "false"
    )]
    pub enable_admin: bool,

    /// RPC server bind address.
    #[arg(
        id = "rpc_addr",
        long = "rpc.addr",
        env = "BASE_PROPOSER_RPC_ADDR",
        default_value = "127.0.0.1"
    )]
    pub addr: IpAddr,

    /// RPC server port.
    #[arg(
        id = "rpc_port",
        long = "rpc.port",
        env = "BASE_PROPOSER_RPC_PORT",
        default_value = "8545"
    )]
    pub port: u16,
}

impl Cli {
    /// Run the proposer service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let config = ProposerConfig::try_from(self)?;
        base_proposer::run(config).await
    }
}

impl TryFrom<Cli> for ProposerConfig {
    type Error = ConfigError;

    fn try_from(cli: Cli) -> Result<Self, Self::Error> {
        validate_url(&cli.proposer.enclave_rpc, "enclave-rpc")?;
        validate_url(&cli.proposer.l1_eth_rpc, "l1-eth-rpc")?;
        validate_url(&cli.proposer.l2_eth_rpc, "l2-eth-rpc")?;
        validate_url(&cli.proposer.rollup_rpc, "rollup-rpc")?;

        if cli.proposer.poll_interval.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "poll-interval",
                constraint: "greater than 0",
                value: "0".to_string(),
            });
        }

        if cli.metrics.enabled && cli.metrics.port == 0 {
            return Err(ConfigError::Metrics(
                "metrics port must be non-zero when metrics are enabled".to_string(),
            ));
        }

        if cli.rpc.enable_admin && cli.rpc.port == 0 {
            return Err(ConfigError::Rpc(
                "RPC port must be non-zero when admin is enabled".to_string(),
            ));
        }

        let signing = build_signing_config(
            cli.proposer.private_key.as_deref(),
            cli.proposer.signer_endpoint.as_ref(),
            cli.proposer.signer_address.as_ref(),
        )?;

        let retry = RetryConfig {
            max_attempts: cli.proposer.rpc_max_retries,
            initial_delay: cli.proposer.rpc_retry_initial_delay,
            max_delay: cli.proposer.rpc_retry_max_delay,
        };

        Ok(Self {
            allow_non_finalized: cli.proposer.allow_non_finalized,
            enclave_rpc: cli.proposer.enclave_rpc,
            l1_eth_rpc: cli.proposer.l1_eth_rpc,
            l2_eth_rpc: cli.proposer.l2_eth_rpc,
            l2_reth: cli.proposer.l2_reth,
            anchor_state_registry_addr: cli.proposer.anchor_state_registry_addr,
            dispute_game_factory_addr: cli.proposer.dispute_game_factory_addr,
            game_type: cli.proposer.game_type,
            tee_image_hash: cli.proposer.tee_image_hash,
            poll_interval: cli.proposer.poll_interval,
            rpc_timeout: cli.proposer.rpc_timeout,
            retry,
            signing,
            rollup_rpc: cli.proposer.rollup_rpc,
            skip_tls_verify: cli.proposer.skip_tls_verify,
            wait_node_sync: cli.proposer.wait_node_sync,
            log: cli.logging.into(),
            metrics: cli.metrics.into(),
            rpc: RpcServerConfig {
                enable_admin: cli.rpc.enable_admin,
                addr: cli.rpc.addr,
                port: cli.rpc.port,
            },
        })
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

/// Parse a 32-byte hash from hex string (0x-prefixed).
fn parse_b256(s: &str) -> Result<B256, alloy_primitives::hex::FromHexError> {
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
            "proposer",
            "--enclave-rpc",
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

        assert!(!cli.proposer.allow_non_finalized);
        assert!(!cli.proposer.l2_reth);
        assert_eq!(cli.proposer.poll_interval, Duration::from_secs(12));
        assert_eq!(cli.proposer.rpc_timeout, Duration::from_secs(30));
        assert_eq!(cli.proposer.rollup_rpc.as_str(), "http://localhost:7545/");
        assert!(!cli.proposer.skip_tls_verify);
        assert!(!cli.proposer.wait_node_sync);
        assert_eq!(cli.proposer.game_type, 1);

        assert_eq!(cli.logging.level, 3);
        assert_eq!(cli.logging.stdout_format, LogFormat::Full);
        assert!(!cli.logging.stdout_quiet);

        assert!(!cli.metrics.enabled);
        assert_eq!(cli.metrics.addr, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(cli.metrics.port, 7300);

        assert!(!cli.rpc.enable_admin);
        assert_eq!(cli.rpc.addr, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(cli.rpc.port, 8545);

        assert_eq!(cli.proposer.rpc_max_retries, 5);
        assert_eq!(cli.proposer.rpc_retry_initial_delay, Duration::from_millis(100));
        assert_eq!(cli.proposer.rpc_retry_max_delay, Duration::from_secs(10));

        assert!(cli.proposer.private_key.is_none());
        assert!(cli.proposer.signer_endpoint.is_none());
        assert!(cli.proposer.signer_address.is_none());
    }

    #[test]
    fn test_cli_missing_required() {
        let args = vec!["proposer"];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn test_cli_missing_rollup_rpc() {
        let args = vec![
            "proposer",
            "--enclave-rpc",
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
