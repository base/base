//! CLI argument definitions for proposer.

use std::{net::IpAddr, time::Duration};

use alloy_primitives::Address;
use base_cli_utils::{CliStyles, LogFormat};
use clap::{ArgAction, Parser};
use url::Url;

/// Proposer - TEE-based output proposal generation for OP Stack chains.
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

    /// RPC server configuration arguments.
    #[command(flatten)]
    pub rpc: RpcServerArgs,
}

/// Core proposer configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Proposer")]
pub struct ProposerArgs {
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
    #[arg(
        long = "l2-reth",
        env = "BASE_PROPOSER_L2_RETH",
        default_value = "false"
    )]
    pub l2_reth: bool,

    /// Minimum number of blocks between proposals.
    #[arg(
        long = "min-proposal-interval",
        env = "BASE_PROPOSER_MIN_PROPOSAL_INTERVAL",
        default_value = "512"
    )]
    pub min_proposal_interval: u64,

    /// Address of the on-chain verifier contract.
    #[arg(
        long = "onchain-verifier-addr",
        env = "BASE_PROPOSER_ONCHAIN_VERIFIER_ADDR",
        value_parser = parse_address
    )]
    pub onchain_verifier_addr: Address,

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
    #[arg(
        long = "wait-node-sync",
        env = "BASE_PROPOSER_WAIT_NODE_SYNC",
        default_value = "false"
    )]
    pub wait_node_sync: bool,

    /// Maximum number of retry attempts for RPC operations.
    #[arg(
        long = "rpc-max-retries",
        env = "BASE_PROPOSER_RPC_MAX_RETRIES",
        default_value = "5"
    )]
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
}

/// Logging configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Logging")]
pub struct LogArgs {
    /// Increase logging verbosity (1=ERROR, 2=WARN, 3=INFO, 4=DEBUG, 5=TRACE).
    #[arg(
        short = 'v',
        long = "verbose",
        action = ArgAction::Count,
        default_value = "3",
        env = "BASE_PROPOSER_LOG_LEVEL",
        global = true
    )]
    pub level: u8,

    /// Suppress stdout logging.
    #[arg(long = "quiet", short = 'q', global = true)]
    pub stdout_quiet: bool,

    /// Stdout log format.
    #[arg(
        long = "log-format",
        default_value = "full",
        env = "BASE_PROPOSER_LOG_FORMAT",
        global = true
    )]
    pub stdout_format: LogFormat,
}

/// Metrics server configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Metrics")]
pub struct MetricsArgs {
    /// Enable metrics server.
    #[arg(
        id = "metrics_enabled",
        long = "metrics.enabled",
        env = "BASE_PROPOSER_METRICS_ENABLED",
        default_value = "false"
    )]
    pub enabled: bool,

    /// Metrics server bind address.
    #[arg(
        id = "metrics_addr",
        long = "metrics.addr",
        env = "BASE_PROPOSER_METRICS_ADDR",
        default_value = "0.0.0.0"
    )]
    pub addr: IpAddr,

    /// Metrics server port.
    #[arg(
        id = "metrics_port",
        long = "metrics.port",
        env = "BASE_PROPOSER_METRICS_PORT",
        default_value = "7300"
    )]
    pub port: u16,
}

/// RPC server configuration arguments.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "RPC Server")]
pub struct RpcServerArgs {
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
        assert_eq!(
            addr.to_string(),
            "0x1234567890123456789012345678901234567890"
        );
    }

    #[test]
    fn test_parse_address_invalid() {
        assert!(parse_address("0xnotanaddress").is_err());
        assert!(parse_address("invalid").is_err());
    }

    #[test]
    fn test_cli_defaults() {
        // Test that we can construct minimal CLI args (requires all required fields)
        let args = vec![
            "proposer",
            "--enclave-rpc",
            "http://localhost:8080",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--onchain-verifier-addr",
            "0x1234567890123456789012345678901234567890",
            "--rollup-rpc",
            "http://localhost:7545",
        ];
        let cli = Cli::try_parse_from(args).unwrap();

        // Check defaults
        assert!(!cli.proposer.allow_non_finalized);
        assert!(!cli.proposer.l2_reth);
        assert_eq!(cli.proposer.min_proposal_interval, 512);
        assert_eq!(cli.proposer.poll_interval, Duration::from_secs(12));
        assert_eq!(cli.proposer.rpc_timeout, Duration::from_secs(30));
        assert_eq!(cli.proposer.rollup_rpc.as_str(), "http://localhost:7545/");
        assert!(!cli.proposer.skip_tls_verify);
        assert!(!cli.proposer.wait_node_sync);

        assert_eq!(cli.logging.level, 3);
        assert_eq!(cli.logging.stdout_format, LogFormat::Full);
        assert!(!cli.logging.stdout_quiet);

        assert!(!cli.metrics.enabled);
        assert_eq!(cli.metrics.addr, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(cli.metrics.port, 7300);

        assert!(!cli.rpc.enable_admin);
        assert_eq!(cli.rpc.addr, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(cli.rpc.port, 8545);

        // Check retry defaults
        assert_eq!(cli.proposer.rpc_max_retries, 5);
        assert_eq!(
            cli.proposer.rpc_retry_initial_delay,
            Duration::from_millis(100)
        );
        assert_eq!(cli.proposer.rpc_retry_max_delay, Duration::from_secs(10));
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
            "--enclave-rpc",
            "http://localhost:8080",
            "--l1-eth-rpc",
            "http://localhost:8545",
            "--l2-eth-rpc",
            "http://localhost:9545",
            "--onchain-verifier-addr",
            "0x1234567890123456789012345678901234567890",
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }
}
