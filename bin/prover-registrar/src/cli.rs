//! CLI argument parsing and config construction for the prover registrar.

use std::{num::NonZeroUsize, time::Duration};

use alloy_primitives::{Address, hex::FromHex};
use base_proof_tee_nitro_attestation_prover::BoundlessProver;
use base_proof_tee_registrar::{
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, RegistrarConfig, RegistrarError, RegistrarService,
};
use base_tx_manager::{SignerConfig, TxManagerConfig};
use boundless_market::{
    alloy::signers::local::PrivateKeySigner,
    price_oracle::{Amount, Asset},
};
use clap::{Args, Parser};
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
    #[command(flatten)]
    boundless: BoundlessArgs,
    /// Interval between discovery and registration poll cycles, in seconds.
    #[arg(
        long,
        env = cli_env!("POLL_INTERVAL_SECS"),
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    poll_interval_secs: u64,

    /// Timeout for JSON-RPC calls to prover instances, in seconds.
    #[arg(
        long,
        env = cli_env!("PROVER_TIMEOUT_SECS"),
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    prover_timeout_secs: u64,

    /// Maximum number of instances to process concurrently within a single
    /// registration cycle. Each instance may trigger a ~20-minute proof
    /// generation, so this limits concurrent proof work.
    #[arg(
        long,
        env = cli_env!("MAX_CONCURRENCY"),
        default_value_t = NonZeroUsize::new(DEFAULT_MAX_CONCURRENCY).unwrap()
    )]
    max_concurrency: NonZeroUsize,
    /// Maximum number of transaction submission retries for transient errors.
    #[arg(long, env = cli_env!("MAX_TX_RETRIES"), default_value_t = DEFAULT_MAX_TX_RETRIES)]
    max_tx_retries: u32,

    /// Delay between transaction submission retries, in seconds.
    #[arg(
        long,
        env = cli_env!("TX_RETRY_DELAY_SECS"),
        default_value_t = DEFAULT_TX_RETRY_DELAY_SECS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    tx_retry_delay_secs: u64,
    /// Duration (seconds) after EC2 launch during which unhealthy instances
    /// are still eligible for registration. New instances may fail ALB health
    /// checks while the application initializes. Set to 0 to disable.
    #[arg(long, env = cli_env!("UNHEALTHY_REGISTRATION_WINDOW_SECS"), default_value_t = DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS)]
    unhealthy_registration_window_secs: u64,
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

/// Boundless Network CLI arguments.
#[derive(Args)]
struct BoundlessArgs {
    /// Boundless Network RPC URL.
    #[arg(long = "boundless-rpc-url", env = cli_env!("BOUNDLESS_RPC_URL"))]
    rpc_url: Url,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long = "boundless-private-key", env = cli_env!("BOUNDLESS_PRIVATE_KEY"))]
    fee_private_key: PrivateKeySigner,

    /// HTTP(S) URL of the Nitro attestation verifier ELF (e.g. Pinata IPFS gateway URL).
    #[arg(
        long = "boundless-verifier-program-url",
        env = cli_env!("BOUNDLESS_VERIFIER_PROGRAM_URL")
    )]
    verifier_program_url: Url,

    /// Interval between Boundless fulfillment status checks, in seconds.
    #[arg(
        long = "boundless-poll-interval-secs",
        env = cli_env!("BOUNDLESS_POLL_INTERVAL_SECS"),
        default_value_t = 5
    )]
    fulfillment_poll_interval_secs: u64,

    /// Client-side fulfillment poll budget, in seconds.
    #[arg(
        long = "boundless-timeout-secs",
        env = cli_env!("BOUNDLESS_TIMEOUT_SECS"),
        default_value_t = 1260,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    timeout_secs: u64,

    /// Minimum Boundless offer price in ETH for each submitted proof request.
    #[arg(
        long = "boundless-min-price-eth",
        env = cli_env!("BOUNDLESS_MIN_PRICE_ETH"),
        requires = "max_price_eth",
        value_parser = parse_boundless_eth_amount
    )]
    min_price_eth: Option<Amount>,

    /// Maximum Boundless offer price in ETH for each submitted proof request.
    #[arg(
        long = "boundless-max-price-eth",
        env = cli_env!("BOUNDLESS_MAX_PRICE_ETH"),
        requires = "min_price_eth",
        value_parser = parse_boundless_eth_amount
    )]
    max_price_eth: Option<Amount>,

    /// Optional duration in seconds for the Boundless offer price ramp.
    #[arg(
        long = "boundless-offer-ramp-up-period-secs",
        env = cli_env!("BOUNDLESS_OFFER_RAMP_UP_PERIOD_SECS")
    )]
    offer_ramp_up_period_secs: Option<u32>,

    /// Maximum lock and delivery window for a Boundless proof request.
    #[arg(
        long = "boundless-offer-lock-timeout-secs",
        env = cli_env!("BOUNDLESS_OFFER_LOCK_TIMEOUT_SECS")
    )]
    offer_lock_timeout_secs: Option<u32>,

    /// Delay before Boundless bidding starts.
    #[arg(
        long = "boundless-offer-bidding-start-delay-secs",
        env = cli_env!("BOUNDLESS_OFFER_BIDDING_START_DELAY_SECS"),
        default_value_t = 0
    )]
    offer_bidding_start_delay_secs: u64,

    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    #[arg(
        long = "boundless-max-recovery-attempts",
        env = cli_env!("BOUNDLESS_MAX_RECOVERY_ATTEMPTS"),
        default_value_t = 5
    )]
    max_recovery_attempts: u32,

    /// Maximum age (in seconds) of a recovered proof's attestation timestamp
    /// before it is considered stale. Should be slightly below the onchain
    /// `MAX_AGE` to account for clock skew. Defaults to 3300 s (55 minutes).
    #[arg(
        long = "max-attestation-age-secs",
        env = cli_env!("MAX_ATTESTATION_AGE_SECS"),
        default_value_t = 3300
    )]
    max_attestation_age_secs: u64,
}

/// Parse a hex-encoded image ID string into `[u32; 8]`.
fn parse_image_id(s: &str) -> std::result::Result<[u32; 8], String> {
    let bytes = <[u8; 32]>::from_hex(s.strip_prefix("0x").unwrap_or(s))
        .map_err(|e| format!("--image-id: {e}"))?;

    Ok(std::array::from_fn(|i| u32::from_le_bytes(bytes[i * 4..][..4].try_into().unwrap())))
}

/// Parse an ETH-denominated Boundless offer price.
fn parse_boundless_eth_amount(s: &str) -> std::result::Result<Amount, String> {
    Amount::parse_with_allowed(s, &[Asset::ETH], Some(Asset::ETH))
        .map_err(|e| format!("Boundless ETH amount: {e}"))
}

impl Cli {
    fn config(self) -> std::result::Result<RegistrarConfig, RegistrarError> {
        let boundless = self.boundless;
        if matches!(
            (&boundless.min_price_eth, &boundless.max_price_eth),
            (Some(min_price), Some(max_price)) if max_price.value < min_price.value
        ) {
            return Err(RegistrarError::Config(
                "--boundless-max-price-eth must be greater than or equal to --boundless-min-price-eth"
                    .into(),
            ));
        }

        let boundless_prover = BoundlessProver {
            offer_min_price: boundless.min_price_eth,
            offer_max_price: boundless.max_price_eth,
            offer_ramp_up_period_secs: boundless.offer_ramp_up_period_secs,
            offer_lock_timeout_secs: boundless.offer_lock_timeout_secs,
            offer_bidding_start_delay_secs: boundless.offer_bidding_start_delay_secs,
            ..BoundlessProver::new(
                boundless.rpc_url,
                boundless.fee_private_key,
                boundless.verifier_program_url,
                self.image_id,
                Duration::from_secs(boundless.fulfillment_poll_interval_secs),
                Duration::from_secs(boundless.timeout_secs),
                1,
                boundless.max_recovery_attempts,
                Duration::from_secs(boundless.max_attestation_age_secs),
            )
        };
        let signing = SignerConfig::try_from(self.signer)
            .map_err(|e| RegistrarError::Config(format!("signer: {e}")))?;
        let tx_manager_config = TxManagerConfig::try_from(self.tx_manager)
            .map_err(|e| RegistrarError::Config(format!("tx-manager: {e}")))?;

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            target_group_arn: self.target_group_arn,
            aws_region: self.aws_region,
            prover_port: self.prover_port,
            signing,
            tx_manager_config,
            boundless_prover,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            prover_timeout: Duration::from_secs(self.prover_timeout_secs),
            max_concurrency: self.max_concurrency.get(),
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: Duration::from_secs(self.tx_retry_delay_secs),
            unhealthy_registration_window: Duration::from_secs(
                self.unhealthy_registration_window_secs,
            ),
            crl_nitro_verifier_address: self.crl_nitro_verifier_address,
            health_addr: self.health.socket_addr(),
            log_config: self.log.into(),
            metrics_config: self.metrics.into(),
        })
    }

    /// Run the registrar service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        RegistrarService::run(self.config()?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_IMAGE_ID: &str =
        "0x0100000002000000030000000400000005000000060000000700000008000000";

    fn boundless_args() -> Vec<&'static str> {
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
            "--image-id",
            TEST_IMAGE_ID,
            "--boundless-rpc-url",
            "http://localhost:9545",
            "--boundless-private-key",
            "0202020202020202020202020202020202020202020202020202020202020202",
            "--boundless-verifier-program-url",
            "https://gateway.pinata.cloud/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ]
    }

    #[test]
    fn boundless_offer_pricing_parses_eth_amounts() {
        let mut args = boundless_args();
        args.extend([
            "--boundless-min-price-eth",
            "0.01",
            "--boundless-max-price-eth",
            "0.03",
            "--boundless-offer-ramp-up-period-secs",
            "30",
        ]);

        let b = Cli::parse_from(args).config().unwrap().boundless_prover;

        assert_eq!(b.offer_min_price, Some(parse_boundless_eth_amount("0.01").unwrap()),);
        assert_eq!(b.offer_max_price, Some(parse_boundless_eth_amount("0.03").unwrap()),);
        assert_eq!(b.offer_ramp_up_period_secs, Some(30),);
    }

    #[test]
    fn boundless_offer_max_price_must_cover_min_price() {
        let mut args = boundless_args();
        args.extend(["--boundless-min-price-eth", "0.03", "--boundless-max-price-eth", "0.01"]);

        let result = Cli::parse_from(args).config();

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
