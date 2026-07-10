//! CLI argument parsing and config construction for the prover registrar.

use std::time::Duration;

use alloy_primitives::{Address, hex::FromHex};
use base_proof_tee_nitro_attestation_prover::BoundlessProver as NitroBoundlessProver;
use base_proof_tee_registrar::{
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, INSTANCE_CACHE_TTL_CYCLES, PlatformProofProvider,
    RegistrarConfig, RegistrarError,
};
use base_proof_tee_tdx_attestation_prover::BoundlessProver as TdxBoundlessProver;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use boundless_market::alloy::signers::local::PrivateKeySigner;
use clap::Parser;
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

    /// GCP project ID containing the TDX prover GKE cluster.
    #[arg(long, env = cli_env!("GCP_PROJECT"))]
    gcp_project: String,

    /// GCP location containing the TDX prover GKE cluster.
    #[arg(long, env = cli_env!("GCP_LOCATION"))]
    gcp_location: String,

    /// GKE cluster name containing the TDX prover node pool.
    #[arg(long, env = cli_env!("GCP_CLUSTER"))]
    gcp_cluster: String,

    /// GKE node pool name for TDX prover nodes.
    #[arg(long, env = cli_env!("GCP_NODE_POOL"))]
    gcp_node_pool: String,

    /// Optional GCP `OAuth` access token. If unset, the GCP metadata server is used.
    #[arg(long, env = cli_env!("GCP_ACCESS_TOKEN"))]
    gcp_access_token: Option<String>,

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = cli_env!("PROVER_PORT"), default_value_t = 8000)]
    prover_port: u16,

    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    signer: SignerCli,

    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    tx_manager: TxManagerCli,

    /// Hex-encoded Nitro verifier guest program image ID.
    #[arg(long = "nitro-image-id", alias = "image-id", env = cli_env!("IMAGE_ID"), value_parser = parse_image_id)]
    nitro_image_id: [u32; 8],

    /// Hex-encoded TDX verifier guest program image ID.
    #[arg(long = "tdx-image-id", env = cli_env!("TDX_IMAGE_ID"), value_parser = parse_image_id)]
    tdx_image_id: [u32; 8],

    /// Boundless Network RPC URL.
    #[arg(long, env = cli_env!("BOUNDLESS_RPC_URL"))]
    boundless_rpc_url: Url,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long = "boundless-private-key", env = cli_env!("BOUNDLESS_PRIVATE_KEY"))]
    boundless_fee_private_key: PrivateKeySigner,

    /// HTTP(S) URL of the Nitro attestation verifier ELF.
    #[arg(
        long = "boundless-nitro-verifier-program-url",
        alias = "boundless-verifier-program-url",
        env = cli_env!("BOUNDLESS_VERIFIER_PROGRAM_URL")
    )]
    boundless_nitro_verifier_program_url: Url,

    /// HTTP(S) URL of the TDX attestation verifier ELF.
    #[arg(long = "boundless-tdx-verifier-program-url", env = cli_env!("BOUNDLESS_TDX_VERIFIER_PROGRAM_URL"))]
    boundless_tdx_verifier_program_url: Url,

    /// Boundless fulfillment poll interval in seconds.
    #[arg(
        long = "boundless-poll-interval-secs",
        env = cli_env!("BOUNDLESS_POLL_INTERVAL_SECS"),
        default_value_t = 5,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    boundless_fulfillment_poll_interval: u64,

    /// Boundless fulfillment timeout in seconds.
    #[arg(
        long = "boundless-timeout-secs",
        env = cli_env!("BOUNDLESS_TIMEOUT_SECS"),
        default_value_t = 1260,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    boundless_timeout: u64,

    /// Maximum request-ID slots to probe during proof recovery.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MAX_RECOVERY_ATTEMPTS"),
        default_value_t = 5
    )]
    boundless_max_recovery_attempts: u32,

    /// Maximum recovered attestation age in seconds.
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

    /// Grace period for registering newly launched unhealthy instances.
    #[arg(
        long = "unhealthy-registration-window-secs",
        env = cli_env!("UNHEALTHY_REGISTRATION_WINDOW_SECS"),
        default_value_t = DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS
    )]
    unhealthy_registration_window: u64,

    /// `NitroEnclaveVerifier` contract address for CRL checks. Providing this enables CRL checks.
    #[arg(long, env = cli_env!("CRL_NITRO_VERIFIER_ADDRESS"))]
    crl_nitro_verifier_address: Option<Address>,

    #[command(flatten)]
    health: HealthArgs,

    #[command(flatten)]
    log: LogArgs,

    #[command(flatten)]
    metrics: MetricsArgs,
}

/// Parse a hex-encoded image ID string into `[u32; 8]`.
fn parse_image_id(s: &str) -> Result<[u32; 8], String> {
    let bytes = <[u8; 32]>::from_hex(s.strip_prefix("0x").unwrap_or(s))
        .map_err(|e| format!("--image-id: {e}"))?;

    Ok(std::array::from_fn(|i| u32::from_le_bytes(bytes[i * 4..][..4].try_into().unwrap())))
}

impl Cli {
    pub(crate) fn config(self) -> Result<RegistrarConfig, Box<RegistrarError>> {
        validate_health_port(self.health.port)?;

        let boundless_rpc_url = self.boundless_rpc_url;
        let boundless_signer = self.boundless_fee_private_key;
        let boundless_signer_address = boundless_signer.address();
        let boundless_timing = (
            Duration::from_secs(self.boundless_fulfillment_poll_interval),
            Duration::from_secs(self.boundless_timeout),
        );
        let max_attestation_age = Duration::from_secs(self.max_attestation_age);

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            target_group_arn: self.target_group_arn,
            aws_region: self.aws_region,
            gcp_project: self.gcp_project,
            gcp_location: self.gcp_location,
            gcp_cluster: self.gcp_cluster,
            gcp_node_pool: self.gcp_node_pool,
            gcp_access_token: self.gcp_access_token,
            prover_port: self.prover_port,
            signing: SignerConfig::try_from(self.signer)
                .map_err(|e| Box::new(RegistrarError::Config(format!("signer: {e}"))))?,
            tx_manager_config: TxManagerConfig::try_from(self.tx_manager)
                .map_err(|e| Box::new(RegistrarError::Config(format!("tx-manager: {e}"))))?,
            proof_provider: PlatformProofProvider::new(
                NitroBoundlessProver::new(
                    boundless_rpc_url.clone(),
                    boundless_signer.clone(),
                    self.boundless_nitro_verifier_program_url,
                    self.nitro_image_id,
                    boundless_timing,
                    self.boundless_max_recovery_attempts,
                    max_attestation_age,
                ),
                TdxBoundlessProver::new(
                    boundless_rpc_url.clone(),
                    boundless_signer,
                    self.boundless_tdx_verifier_program_url,
                    self.tdx_image_id,
                    boundless_timing,
                    self.boundless_max_recovery_attempts,
                    max_attestation_age,
                ),
            ),
            boundless_rpc_url,
            boundless_signer_address,
            max_attestation_age,
            poll_interval: Duration::from_secs(self.poll_interval),
            prover_timeout: Duration::from_secs(self.prover_timeout),
            max_concurrency: self.max_concurrency,
            instance_cache_ttl_cycles: self.instance_cache_ttl_cycles,
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: Duration::from_secs(self.tx_retry_delay),
            unhealthy_registration_window: Duration::from_secs(self.unhealthy_registration_window),
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

    const TEST_IMAGE_ID: &str =
        "0x0100000002000000030000000400000005000000060000000700000008000000";

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
            "--gcp-project",
            "base-project",
            "--gcp-location",
            "us-central1",
            "--gcp-cluster",
            "tee-provers",
            "--gcp-node-pool",
            "tdx-provers",
            "--private-key",
            "0x0101010101010101010101010101010101010101010101010101010101010101",
            "--nitro-image-id",
            TEST_IMAGE_ID,
            "--tdx-image-id",
            TEST_IMAGE_ID,
            "--boundless-rpc-url",
            "http://localhost:9545",
            "--boundless-private-key",
            "0x0202020202020202020202020202020202020202020202020202020202020202",
            "--boundless-nitro-verifier-program-url",
            "https://gateway.pinata.cloud/ipfs/nitro-test",
            "--boundless-tdx-verifier-program-url",
            "https://gateway.pinata.cloud/ipfs/tdx-test",
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
            "--boundless-timeout-secs",
            "1260",
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
            "--unhealthy-registration-window-secs",
            "600",
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

    #[test]
    fn parse_image_id_valid() {
        for input in [TEST_IMAGE_ID, TEST_IMAGE_ID.trim_start_matches("0x")] {
            assert_eq!(parse_image_id(input).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        }
    }

    #[test]
    fn parse_image_id_invalid() {
        for input in ["00000001", "zzzz", ""] {
            assert!(parse_image_id(input).is_err());
        }
    }
}
