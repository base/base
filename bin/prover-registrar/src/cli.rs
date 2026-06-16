//! CLI argument parsing and config construction for the prover registrar.

use std::time::Duration;

use alloy_primitives::{Address, hex::FromHex};
use base_proof_tee_nitro_attestation_prover::BoundlessProver;
use base_proof_tee_registrar::{
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, RegistrarConfig, RegistrarError,
};
use base_tx_manager::{SignerConfig, TxManagerConfig};
use boundless_market::{
    alloy::signers::local::PrivateKeySigner,
    price_oracle::{Amount, Asset},
};
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

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = cli_env!("PROVER_PORT"), default_value_t = 8000)]
    prover_port: u16,
    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    signer: SignerCli,
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    tx_manager: TxManagerCli,

    /// Hex-encoded guest program image ID.
    #[arg(long, env = cli_env!("IMAGE_ID"), value_parser = parse_image_id)]
    image_id: [u32; 8],

    /// Boundless Network RPC URL.
    #[arg(long, env = cli_env!("BOUNDLESS_RPC_URL"))]
    boundless_rpc_url: Url,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long = "boundless-private-key", env = cli_env!("BOUNDLESS_PRIVATE_KEY"))]
    boundless_fee_private_key: PrivateKeySigner,

    /// HTTP(S) URL of the Nitro attestation verifier ELF (e.g. Pinata IPFS gateway URL).
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_VERIFIER_PROGRAM_URL")
    )]
    boundless_verifier_program_url: Url,

    /// Interval between Boundless fulfillment status checks, in seconds.
    #[arg(
        long = "boundless-poll-interval-secs",
        env = cli_env!("BOUNDLESS_POLL_INTERVAL_SECS"),
        default_value = "5",
        value_parser = parse_duration
    )]
    boundless_fulfillment_poll_interval: Duration,

    /// Client-side fulfillment poll budget, in seconds.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_TIMEOUT_SECS"),
        default_value = "1260",
        value_parser = parse_nonzero_duration
    )]
    boundless_timeout: Duration,

    /// Minimum Boundless offer price in ETH for each submitted proof request.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MIN_PRICE_ETH"),
        requires = "boundless_max_price_eth",
        value_parser = parse_boundless_eth_amount
    )]
    boundless_min_price_eth: Option<Amount>,

    /// Maximum Boundless offer price in ETH for each submitted proof request.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MAX_PRICE_ETH"),
        requires = "boundless_min_price_eth",
        value_parser = parse_boundless_eth_amount
    )]
    boundless_max_price_eth: Option<Amount>,

    /// Optional duration in seconds for the Boundless offer price ramp.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_OFFER_RAMP_UP_PERIOD_SECS")
    )]
    boundless_offer_ramp_up_period_secs: Option<u32>,

    /// Maximum lock and delivery window for a Boundless proof request.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_OFFER_LOCK_TIMEOUT_SECS")
    )]
    boundless_offer_lock_timeout_secs: Option<u32>,

    /// Delay before Boundless bidding starts.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_OFFER_BIDDING_START_DELAY_SECS"),
        default_value_t = 0
    )]
    boundless_offer_bidding_start_delay_secs: u64,

    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MAX_RECOVERY_ATTEMPTS"),
        default_value_t = 5
    )]
    boundless_max_recovery_attempts: u32,

    /// Maximum age (in seconds) of a recovered proof's attestation timestamp
    /// before it is considered stale. Should be slightly below the onchain
    /// `MAX_AGE` to account for clock skew. Defaults to 3300 s (55 minutes).
    #[arg(
        long,
        env = cli_env!("MAX_ATTESTATION_AGE_SECS"),
        default_value = "3300",
        value_parser = parse_duration
    )]
    max_attestation_age: Duration,

    /// Interval between discovery and registration poll cycles, in seconds.
    #[arg(
        long,
        env = cli_env!("POLL_INTERVAL_SECS"),
        default_value = "30",
        value_parser = parse_nonzero_duration
    )]
    poll_interval: Duration,

    /// Timeout for JSON-RPC calls to prover instances, in seconds.
    #[arg(
        long,
        env = cli_env!("PROVER_TIMEOUT_SECS"),
        default_value = "30",
        value_parser = parse_nonzero_duration
    )]
    prover_timeout: Duration,

    /// Maximum number of instances to process concurrently within a single
    /// registration cycle. Each instance may trigger a ~20-minute proof
    /// generation, so this limits concurrent proof work.
    #[arg(
        long,
        env = cli_env!("MAX_CONCURRENCY"),
        default_value_t = DEFAULT_MAX_CONCURRENCY,
        value_parser = parse_nonzero_usize
    )]
    max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    #[arg(long, env = cli_env!("MAX_TX_RETRIES"), default_value_t = DEFAULT_MAX_TX_RETRIES)]
    max_tx_retries: u32,

    /// Delay between transaction submission retries, in seconds.
    #[arg(
        long,
        env = cli_env!("TX_RETRY_DELAY_SECS"),
        default_value = "5",
        value_parser = parse_nonzero_duration
    )]
    tx_retry_delay: Duration,
    /// Duration (seconds) after EC2 launch during which unhealthy instances
    /// are still eligible for registration. New instances may fail ALB health
    /// checks while the application initializes. Set to 0 to disable.
    #[arg(
        long,
        env = cli_env!("UNHEALTHY_REGISTRATION_WINDOW_SECS"),
        default_value = "5100",
        value_parser = parse_duration
    )]
    unhealthy_registration_window: Duration,
    /// `NitroEnclaveVerifier` contract address for CRL checks.
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

/// Parse an ETH-denominated Boundless offer price.
fn parse_boundless_eth_amount(s: &str) -> Result<Amount, String> {
    Amount::parse_with_allowed(s, &[Asset::ETH], Some(Asset::ETH))
        .map_err(|e| format!("Boundless ETH amount: {e}"))
}

/// Parse a duration, accepting bare numbers as seconds for existing `*_SECS` env vars.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<u64>().map(Duration::from_secs).map_err(|e| e.to_string());
    }

    humantime::parse_duration(s).map_err(|e| e.to_string())
}

fn parse_nonzero_duration(s: &str) -> Result<Duration, String> {
    let duration = parse_duration(s)?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".into());
    }
    Ok(duration)
}

fn parse_nonzero_usize(s: &str) -> Result<usize, String> {
    let value = s.parse::<usize>().map_err(|e| e.to_string())?;
    if value == 0 {
        return Err("value must be greater than zero".into());
    }
    Ok(value)
}

impl Cli {
    pub(crate) fn config(self) -> Result<RegistrarConfig, RegistrarError> {
        if matches!(
            (&self.boundless_min_price_eth, &self.boundless_max_price_eth),
            (Some(min_price), Some(max_price)) if max_price.value < min_price.value
        ) {
            return Err(RegistrarError::Config(
                "--boundless-max-price-eth must be greater than or equal to --boundless-min-price-eth"
                    .into(),
            ));
        }

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            target_group_arn: self.target_group_arn,
            aws_region: self.aws_region,
            prover_port: self.prover_port,
            signing: SignerConfig::try_from(self.signer)
                .map_err(|e| RegistrarError::Config(format!("signer: {e}")))?,
            tx_manager_config: TxManagerConfig::try_from(self.tx_manager)
                .map_err(|e| RegistrarError::Config(format!("tx-manager: {e}")))?,
            boundless_prover: BoundlessProver::new(
                self.boundless_rpc_url,
                self.boundless_fee_private_key,
                self.boundless_verifier_program_url,
                self.image_id,
                self.boundless_fulfillment_poll_interval,
                self.boundless_timeout,
                self.boundless_max_recovery_attempts,
                self.max_attestation_age,
                self.boundless_min_price_eth,
                self.boundless_max_price_eth,
                self.boundless_offer_ramp_up_period_secs,
                self.boundless_offer_lock_timeout_secs,
                self.boundless_offer_bidding_start_delay_secs,
            ),
            poll_interval: self.poll_interval,
            prover_timeout: self.prover_timeout,
            max_concurrency: self.max_concurrency,
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: self.tx_retry_delay,
            unhealthy_registration_window: self.unhealthy_registration_window,
            crl_nitro_verifier_address: self.crl_nitro_verifier_address,
            health_addr: self.health.socket_addr(),
            log_config: self.log.into(),
            metrics_config: self.metrics.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    const TEST_IMAGE_ID: &str =
        "0x0100000002000000030000000400000005000000060000000700000008000000";
    const CONFIG_ARGS: &[&str] = &[
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
        "--image-id",
        TEST_IMAGE_ID,
        "--boundless-rpc-url",
        "http://localhost:9545",
        "--boundless-private-key",
        "0202020202020202020202020202020202020202020202020202020202020202",
        "--boundless-verifier-program-url",
        "https://gateway.pinata.cloud/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
    ];

    #[test]
    fn boundless_offer_pricing_parses_eth_amounts() {
        assert_eq!(
            parse_boundless_eth_amount("0.01").unwrap(),
            Amount::new(U256::from(10_000_000_000_000_000u64), Asset::ETH),
        );
    }

    #[test]
    fn boundless_offer_max_price_must_cover_min_price() {
        let result = Cli::parse_from(CONFIG_ARGS.iter().copied().chain([
            "--boundless-min-price-eth",
            "0.03",
            "--boundless-max-price-eth",
            "0.01",
        ]))
        .config();

        assert!(result.is_err());
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
