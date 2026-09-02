//! CLI argument parsing and config construction for the prover registrar.

use std::time::Duration;

use alloy_primitives::Address;
use base_proof_tee_registrar::{
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    INSTANCE_CACHE_TTL_CYCLES, RegistrarConfig, RegistrarError,
};
use base_tx_manager::{SignerConfig, TxManagerConfig};
use clap::Parser;
use reth_node_core::args::TraceArgs;
use url::Url;

// Generate env-var helper and CLI structs with the `BASE_REGISTRAR_` prefix.
base_cli_utils::define_cli_env!("BASE_REGISTRAR");
base_cli_utils::define_log_args!("BASE_REGISTRAR");
base_cli_utils::define_metrics_args!("BASE_REGISTRAR", 7300);
base_cli_utils::define_health_args!("BASE_REGISTRAR", 8080);
base_tx_manager::define_signer_cli!("BASE_REGISTRAR");
base_tx_manager::define_tx_manager_cli!("BASE_REGISTRAR");

/// Prover Registrar — automated TEE signer registration service.
#[derive(Parser)]
#[command(name = "prover-registrar", version, about)]
pub(crate) struct Cli {
    /// L1 Ethereum RPC endpoint.
    #[arg(long, env = cli_env!("L1_RPC_URL"))]
    l1_rpc_url: Url,

    /// `TEEProverRegistry` contract address on L1.
    #[arg(long, env = cli_env!("TEE_PROVER_REGISTRY_ADDRESS"))]
    tee_prover_registry_address: Address,

    /// AWS ALB target group ARN for prover instance discovery.
    #[arg(long, env = cli_env!("TARGET_GROUP_ARN"))]
    target_group_arn: String,

    /// AWS region (e.g. `us-east-1`).
    #[arg(long, env = cli_env!("AWS_REGION"))]
    aws_region: String,

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = cli_env!("PROVER_PORT"), default_value_t = 8000)]
    prover_port: u16,

    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    signer: SignerCli,

    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    tx_manager: TxManagerCli,

    /// Maximum accepted attestation age in seconds.
    #[arg(
        long = "max-attestation-age-secs",
        env = cli_env!("MAX_ATTESTATION_AGE_SECS"),
        default_value_t = 3300
    )]
    max_attestation_age: u64,

    /// Registration poll interval in seconds.
    #[arg(
        long = "poll-interval-secs",
        env = cli_env!("POLL_INTERVAL_SECS"),
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    poll_interval: u64,

    /// Prover JSON-RPC timeout in seconds.
    #[arg(
        long = "prover-timeout-secs",
        env = cli_env!("PROVER_TIMEOUT_SECS"),
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    prover_timeout: u64,

    /// Maximum instances to process concurrently per registration cycle.
    #[arg(
        long,
        env = cli_env!("MAX_CONCURRENCY"),
        default_value_t = DEFAULT_MAX_CONCURRENCY,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    max_concurrency: usize,

    /// Discovery cycles to preserve signers for instances missing from discovery output.
    ///
    /// Shorter TTLs speed up real cleanup but are more vulnerable to transient AWS/ALB
    /// discovery flakes; longer TTLs protect against flakes but delay cleanup.
    #[arg(
        long,
        env = cli_env!("INSTANCE_CACHE_TTL_CYCLES"),
        default_value_t = INSTANCE_CACHE_TTL_CYCLES
    )]
    instance_cache_ttl_cycles: u32,

    /// Maximum number of transaction submission retries for transient errors.
    #[arg(long, env = cli_env!("MAX_TX_RETRIES"), default_value_t = DEFAULT_MAX_TX_RETRIES)]
    max_tx_retries: u32,

    /// Initial transaction submission retry delay in seconds.
    #[arg(
        long = "tx-retry-delay-secs",
        env = cli_env!("TX_RETRY_DELAY_SECS"),
        default_value_t = DEFAULT_TX_RETRY_DELAY_SECS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    tx_retry_delay: u64,

    /// `NitroEnclaveVerifier` contract address for CRL checks. Providing this enables CRL checks.
    #[arg(long, env = cli_env!("CRL_NITRO_VERIFIER_ADDRESS"))]
    crl_nitro_verifier_address: Option<Address>,

    #[command(flatten)]
    health: HealthArgs,

    #[command(flatten)]
    log: LogArgs,

    #[command(flatten)]
    metrics: MetricsArgs,

    #[command(flatten)]
    pub(crate) traces: TraceArgs,
}

impl Cli {
    pub(crate) fn config(self) -> Result<RegistrarConfig, Box<RegistrarError>> {
        validate_health_port(self.health.port)?;

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            target_group_arn: self.target_group_arn,
            aws_region: self.aws_region,
            prover_port: self.prover_port,
            signing: SignerConfig::try_from(self.signer)
                .map_err(|e| Box::new(RegistrarError::Config(format!("signer: {e}"))))?,
            tx_manager_config: TxManagerConfig::try_from(self.tx_manager)
                .map_err(|e| Box::new(RegistrarError::Config(format!("tx-manager: {e}"))))?,
            max_attestation_age: Duration::from_secs(self.max_attestation_age),
            poll_interval: Duration::from_secs(self.poll_interval),
            prover_timeout: Duration::from_secs(self.prover_timeout),
            max_concurrency: self.max_concurrency,
            instance_cache_ttl_cycles: self.instance_cache_ttl_cycles,
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: Duration::from_secs(self.tx_retry_delay),
            crl_nitro_verifier_address: self.crl_nitro_verifier_address,
            health_addr: self.health.socket_addr(),
            log_config: self.log.into(),
            metrics_config: self.metrics.into(),
        })
    }
}

fn validate_health_port(port: u16) -> Result<(), Box<RegistrarError>> {
    if port == 0 {
        return Err(Box::new(RegistrarError::Config("health server port must be non-zero".into())));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_args() -> Vec<&'static str> {
        vec![
            "prover-registrar",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--tee-prover-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--target-group-arn",
            "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/prover/abc123",
            "--aws-region",
            "us-east-1",
            "--private-key",
            "0x0101010101010101010101010101010101010101010101010101010101010101",
        ]
    }

    #[test]
    fn max_concurrency_zero_rejected() {
        let mut args = required_args();
        args.extend(["--max-concurrency", "0"]);

        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn documented_secs_flag_names_parse() {
        let mut args = required_args();
        args.extend([
            "--max-attestation-age-secs",
            "3300",
            "--poll-interval-secs",
            "30",
            "--prover-timeout-secs",
            "30",
            "--tx-retry-delay-secs",
            "2",
            "--instance-cache-ttl-cycles",
            "3",
        ]);

        assert!(Cli::try_parse_from(args).is_ok());
    }

    #[test]
    fn instance_cache_ttl_cycles_configures_registrar() {
        let mut args = required_args();
        args.extend(["--instance-cache-ttl-cycles", "2"]);

        let config = Cli::parse_from(args).config().unwrap();

        assert_eq!(config.instance_cache_ttl_cycles, 2);
    }

    #[test]
    fn max_attestation_age_configures_registrar() {
        let mut args = required_args();
        args.extend(["--max-attestation-age-secs", "120"]);

        let config = Cli::parse_from(args).config().unwrap();

        assert_eq!(config.max_attestation_age, Duration::from_secs(120));
    }

    #[test]
    fn crl_address_enables_crl() {
        let mut args = required_args();
        args.extend(["--crl-nitro-verifier-address", "0x0000000000000000000000000000000000000099"]);

        let config = Cli::parse_from(args).config().unwrap();

        assert!(config.crl_nitro_verifier_address.is_some());
    }

    #[test]
    fn crl_omitted_disables_crl() {
        let config = Cli::parse_from(required_args()).config().unwrap();

        assert!(config.crl_nitro_verifier_address.is_none());
    }

    #[test]
    fn health_port_zero_rejected() {
        assert!(validate_health_port(0).is_err());
    }
}
