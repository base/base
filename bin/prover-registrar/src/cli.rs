//! CLI argument parsing and config construction for the prover registrar.

use std::{path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_proof_tee_nitro_attestation_prover::{
    AttestationProofProvider, BoundlessProver, DirectProver,
};
use base_proof_tee_registrar::{
    AwsDiscoveryConfig, AwsTargetGroupDiscovery, BoundlessConfig, DriverConfig, ProvingConfig,
    RegistrarConfig, RegistrarError, RegistrationDriver, RegistryContractClient,
};
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use clap::{Args, Parser, ValueEnum};
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

// Generate env-var helper and CLI structs with the `BASE_REGISTRAR_` prefix.
base_cli_utils::define_cli_env!("BASE_REGISTRAR");
base_tx_manager::define_signer_cli!("BASE_REGISTRAR");
base_tx_manager::define_tx_manager_cli!("BASE_REGISTRAR");

/// Default trusted certificate prefix length (root cert only).
const DEFAULT_TRUSTED_CERTS_PREFIX: u8 = 1;

/// Prover Registrar — automated TEE signer registration service.
#[derive(Parser)]
#[command(name = "prover-registrar", version, about)]
pub(crate) struct Cli {
    // ── L1 ────────────────────────────────────────────────────────────────────
    /// L1 Ethereum RPC endpoint.
    #[arg(long, env = cli_env!("L1_RPC_URL"))]
    l1_rpc_url: Url,

    /// `TEEProverRegistry` contract address on L1.
    #[arg(long, env = cli_env!("TEE_PROVER_REGISTRY_ADDRESS"))]
    tee_prover_registry_address: Address,

    /// L1 chain ID (used to validate the RPC connection).
    #[arg(long, env = cli_env!("L1_CHAIN_ID"))]
    l1_chain_id: u64,

    // ── Discovery ─────────────────────────────────────────────────────────────
    /// AWS ALB target group ARN for prover instance discovery.
    #[arg(long, env = cli_env!("TARGET_GROUP_ARN"))]
    target_group_arn: String,

    /// AWS region (e.g. `us-east-1`).
    #[arg(long, env = cli_env!("AWS_REGION"))]
    aws_region: String,

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = cli_env!("PROVER_PORT"), default_value_t = 8000)]
    prover_port: u16,

    // ── Signing ───────────────────────────────────────────────────────────────
    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    signer: SignerCli,

    // ── Transaction Manager ───────────────────────────────────────────────────
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    tx_manager: TxManagerCli,

    // ── Proving ───────────────────────────────────────────────────────────────
    /// ZK proving backend.
    #[arg(long, env = cli_env!("PROVING_MODE"))]
    proving_mode: ProvingMode,

    /// Hex-encoded guest program image ID (required for Boundless mode).
    #[arg(long, env = cli_env!("IMAGE_ID"), required_if_eq("proving_mode", "boundless"))]
    image_id: Option<String>,

    /// Path to the guest ELF binary on disk (required for Direct mode).
    #[arg(long, env = cli_env!("ELF_PATH"), required_if_eq("proving_mode", "direct"))]
    elf_path: Option<PathBuf>,

    // ── Boundless ─────────────────────────────────────────────────────────────
    #[command(flatten)]
    boundless: BoundlessArgs,

    // ── Polling / Server ──────────────────────────────────────────────────────
    /// Interval between discovery and registration poll cycles, in seconds.
    #[arg(long, env = cli_env!("POLL_INTERVAL_SECS"), default_value_t = 30)]
    poll_interval_secs: u64,

    /// Timeout for JSON-RPC calls to prover instances, in seconds.
    #[arg(long, env = cli_env!("PROVER_TIMEOUT_SECS"), default_value_t = 30)]
    prover_timeout_secs: u64,

    /// Port for the health check and Prometheus metrics HTTP server.
    #[arg(long, env = cli_env!("HEALTH_PORT"), default_value_t = 7300)]
    health_port: u16,
}

/// ZK proving backend selector.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum ProvingMode {
    /// Boundless marketplace proving.
    Boundless,
    /// Direct proving via risc0 `default_prover()` (Bonsai remote or dev-mode).
    Direct,
}

/// Boundless Network CLI arguments.
#[derive(Args)]
struct BoundlessArgs {
    /// Boundless Network RPC URL.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_RPC_URL"),
        required_if_eq("proving_mode", "boundless")
    )]
    boundless_rpc_url: Option<Url>,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_PRIVATE_KEY"),
        required_if_eq("proving_mode", "boundless")
    )]
    boundless_private_key: Option<String>,

    /// IPFS URL of the Nitro attestation verifier ELF uploaded via `nitro-attest-cli`.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_VERIFIER_PROGRAM_URL"),
        required_if_eq("proving_mode", "boundless")
    )]
    boundless_verifier_program_url: Option<Url>,

    /// Maximum price in wei per cycle for Boundless proof requests.
    #[arg(long, env = cli_env!("BOUNDLESS_MAX_PRICE"), default_value_t = 1_000_000)]
    boundless_max_price: u64,

    /// Interval between Boundless fulfillment status checks, in seconds.
    #[arg(long, env = cli_env!("BOUNDLESS_POLL_INTERVAL_SECS"), default_value_t = 5)]
    boundless_poll_interval_secs: u64,

    /// Proof generation timeout in seconds.
    #[arg(long, env = cli_env!("BOUNDLESS_TIMEOUT_SECS"), default_value_t = 600)]
    boundless_timeout_secs: u64,

    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    #[arg(long, env = cli_env!("NITRO_VERIFIER_ADDRESS"))]
    nitro_verifier_address: Option<Address>,
}

/// Parse a hex-encoded secp256k1 private key string into a [`PrivateKeySigner`].
fn parse_private_key(
    field: &str,
    s: &str,
) -> std::result::Result<PrivateKeySigner, RegistrarError> {
    s.strip_prefix("0x")
        .unwrap_or(s)
        .parse::<PrivateKeySigner>()
        .map_err(|e| RegistrarError::Config(format!("{field}: {e}")))
}

/// Parse a hex-encoded image ID string into `[u32; 8]`.
fn parse_image_id(s: &str) -> std::result::Result<[u32; 8], RegistrarError> {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    let bytes: [u8; 32] = hex::decode(hex)
        .map_err(|e| RegistrarError::Config(format!("--image-id: {e}")))?
        .try_into()
        .map_err(|v: Vec<u8>| {
            RegistrarError::Config(format!("--image-id: expected 32 bytes, got {}", v.len()))
        })?;

    let mut id = [0u32; 8];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        id[i] = u32::from_be_bytes(chunk.try_into().unwrap());
    }
    Ok(id)
}

impl Cli {
    /// Validate the CLI arguments for logical conflicts and parse into a [`RegistrarConfig`].
    pub(crate) fn into_config(self) -> std::result::Result<RegistrarConfig, RegistrarError> {
        let discovery = AwsDiscoveryConfig {
            target_group_arn: self.target_group_arn,
            aws_region: self.aws_region,
            port: self.prover_port,
        };

        // Convert signing and tx manager config via the macro-generated TryFrom impls.
        let signing = SignerConfig::try_from(self.signer)
            .map_err(|e| RegistrarError::Config(format!("signer: {e}")))?;
        let tx_manager = TxManagerConfig::try_from(self.tx_manager)
            .map_err(|e| RegistrarError::Config(format!("tx-manager: {e}")))?;

        // Build proving config based on mode.
        let proving = match self.proving_mode {
            ProvingMode::Boundless => {
                if self.boundless.boundless_timeout_secs == 0 {
                    return Err(RegistrarError::Config(
                        "--boundless-timeout-secs must be greater than 0".into(),
                    ));
                }

                let boundless_key =
                    self.boundless.boundless_private_key.as_deref().ok_or_else(|| {
                        RegistrarError::Config("--boundless-private-key is required".into())
                    })?;
                let image_id_hex = self
                    .image_id
                    .as_deref()
                    .ok_or_else(|| RegistrarError::Config("--image-id is required".into()))?;

                ProvingConfig::Boundless(Box::new(BoundlessConfig {
                    rpc_url: self.boundless.boundless_rpc_url.ok_or_else(|| {
                        RegistrarError::Config("--boundless-rpc-url is required".into())
                    })?,
                    signer: parse_private_key("--boundless-private-key", boundless_key)?,
                    verifier_program_url: self
                        .boundless
                        .boundless_verifier_program_url
                        .ok_or_else(|| {
                            RegistrarError::Config(
                                "--boundless-verifier-program-url is required".into(),
                            )
                        })?,
                    image_id: parse_image_id(image_id_hex)?,
                    max_price: self.boundless.boundless_max_price,
                    poll_interval: Duration::from_secs(self.boundless.boundless_poll_interval_secs),
                    timeout: Duration::from_secs(self.boundless.boundless_timeout_secs),
                    nitro_verifier_address: self.boundless.nitro_verifier_address,
                }))
            }
            ProvingMode::Direct => {
                let elf_path = self.elf_path.ok_or_else(|| {
                    RegistrarError::Config("--elf-path is required for direct mode".into())
                })?;
                ProvingConfig::Direct { elf_path }
            }
        };

        if self.poll_interval_secs == 0 {
            return Err(RegistrarError::Config(
                "--poll-interval-secs must be greater than 0".into(),
            ));
        }

        if self.prover_timeout_secs == 0 {
            return Err(RegistrarError::Config(
                "--prover-timeout-secs must be greater than 0".into(),
            ));
        }

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            l1_chain_id: self.l1_chain_id,
            discovery,
            signing,
            tx_manager,
            proving,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            prover_timeout: Duration::from_secs(self.prover_timeout_secs),
            health_port: self.health_port,
        })
    }

    /// Run the registrar service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let config = self.into_config()?;

        info!(config = ?config, "starting prover registrar");

        // Build L1 provider.
        let provider = RootProvider::new_http(config.l1_rpc_url.clone());

        let tx_manager = SimpleTxManager::new(
            provider,
            config.signing,
            config.tx_manager,
            config.l1_chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await?;

        // Build AWS SDK clients for discovery.
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.discovery.aws_region.clone()))
            .load()
            .await;
        let elb_client = aws_sdk_elasticloadbalancingv2::Client::new(&aws_config);
        let ec2_client = aws_sdk_ec2::Client::new(&aws_config);

        let discovery = AwsTargetGroupDiscovery::new(
            elb_client,
            ec2_client,
            config.discovery.target_group_arn.clone(),
            config.discovery.port,
        );

        // Build registry client.
        let registry = RegistryContractClient::new(
            config.tee_prover_registry_address,
            config.l1_rpc_url.clone(),
        );

        // Cancel the driver's token on ctrl-c so the inner loop observes
        // shutdown and in-flight operations can complete gracefully.
        let cancel = CancellationToken::new();
        let cancel_on_signal = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("received ctrl-c, shutting down");
            cancel_on_signal.cancel();
        });

        // Build proof provider based on proving mode (type-erased so the
        // driver construction is not duplicated per variant).
        let proof_provider: Box<dyn AttestationProofProvider> = match config.proving {
            ProvingConfig::Boundless(ref boundless) => Box::new(BoundlessProver {
                rpc_url: boundless.rpc_url.clone(),
                signer: boundless.signer.clone(),
                verifier_program_url: boundless.verifier_program_url.clone(),
                image_id: boundless.image_id,
                max_price: boundless.max_price,
                poll_interval: boundless.poll_interval,
                timeout: boundless.timeout,
                trusted_certs_prefix_len: DEFAULT_TRUSTED_CERTS_PREFIX,
            }),
            ProvingConfig::Direct { ref elf_path } => {
                let elf = std::fs::read(elf_path).map_err(|e| {
                    RegistrarError::Config(format!(
                        "failed to read ELF at {}: {e}",
                        elf_path.display()
                    ))
                })?;
                Box::new(DirectProver::new(elf, DEFAULT_TRUSTED_CERTS_PREFIX)?)
            }
        };

        let driver_config = DriverConfig {
            registry_address: config.tee_prover_registry_address,
            poll_interval: config.poll_interval,
            prover_timeout: config.prover_timeout,
            cancel,
        };

        RegistrationDriver::new(discovery, proof_provider, registry, tx_manager, driver_config)
            .run()
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;

    use super::*;

    // ── Shared test constants ───────────────────────────────────────────

    const TEST_L1_RPC: &str = "http://localhost:8545";
    const TEST_L1_CHAIN_ID: &str = "1";
    const TEST_REGISTRY_ADDR: &str = "0x0000000000000000000000000000000000000001";
    const TEST_TARGET_GROUP_ARN: &str =
        "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/prover/abc123";
    const TEST_AWS_REGION: &str = "us-east-1";
    const TEST_PRIVATE_KEY: &str =
        "0x0101010101010101010101010101010101010101010101010101010101010101";
    const TEST_BOUNDLESS_RPC: &str = "http://localhost:9545";
    const TEST_BOUNDLESS_KEY: &str =
        "0202020202020202020202020202020202020202020202020202020202020202";
    const TEST_VERIFIER_URL: &str =
        "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    const TEST_IMAGE_ID: &str =
        "0x0000000100000002000000030000000400000005000000060000000700000008";
    const TEST_ELF_PATH: &str = "/tmp/guest.elf";
    const TEST_SIGNER_ENDPOINT: &str = "http://localhost:8546";
    const TEST_SIGNER_ADDR: &str = "0x0000000000000000000000000000000000000002";

    const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
    const DEFAULT_PROVER_TIMEOUT_SECS: u64 = 30;
    const DEFAULT_PROVER_PORT: u16 = 8000;

    // ── Arg builders ────────────────────────────────────────────────────

    /// Common args shared by all modes (L1, discovery, signing via local key).
    fn common_args() -> Vec<&'static str> {
        vec![
            "prover-registrar",
            "--l1-rpc-url",
            TEST_L1_RPC,
            "--l1-chain-id",
            TEST_L1_CHAIN_ID,
            "--tee-prover-registry-address",
            TEST_REGISTRY_ADDR,
            "--target-group-arn",
            TEST_TARGET_GROUP_ARN,
            "--aws-region",
            TEST_AWS_REGION,
            "--private-key",
            TEST_PRIVATE_KEY,
        ]
    }

    /// Boundless-mode args: common + boundless proving.
    fn boundless_args() -> Vec<&'static str> {
        let mut args = common_args();
        args.extend([
            "--proving-mode",
            "boundless",
            "--image-id",
            TEST_IMAGE_ID,
            "--boundless-rpc-url",
            TEST_BOUNDLESS_RPC,
            "--boundless-private-key",
            TEST_BOUNDLESS_KEY,
            "--boundless-verifier-program-url",
            TEST_VERIFIER_URL,
        ]);
        args
    }

    /// Direct-mode args: common + direct proving.
    fn direct_args() -> Vec<&'static str> {
        let mut args = common_args();
        args.extend(["--proving-mode", "direct", "--elf-path", TEST_ELF_PATH]);
        args
    }

    /// Remote signer + boundless proving.
    fn remote_signer_args() -> Vec<&'static str> {
        vec![
            "prover-registrar",
            "--l1-rpc-url",
            TEST_L1_RPC,
            "--l1-chain-id",
            TEST_L1_CHAIN_ID,
            "--tee-prover-registry-address",
            TEST_REGISTRY_ADDR,
            "--target-group-arn",
            TEST_TARGET_GROUP_ARN,
            "--aws-region",
            TEST_AWS_REGION,
            "--signer-endpoint",
            TEST_SIGNER_ENDPOINT,
            "--signer-address",
            TEST_SIGNER_ADDR,
            "--proving-mode",
            "boundless",
            "--image-id",
            TEST_IMAGE_ID,
            "--boundless-rpc-url",
            TEST_BOUNDLESS_RPC,
            "--boundless-private-key",
            TEST_BOUNDLESS_KEY,
            "--boundless-verifier-program-url",
            TEST_VERIFIER_URL,
        ]
    }

    // ── Happy-path parsing ──────────────────────────────────────────────

    #[rstest]
    #[case::boundless(boundless_args())]
    #[case::direct(direct_args())]
    #[case::remote_signer(remote_signer_args())]
    fn valid_config_parses(#[case] args: Vec<&str>) {
        assert!(Cli::parse_from(args).into_config().is_ok());
    }

    // ── Proving mode variants ───────────────────────────────────────────

    #[rstest]
    fn boundless_mode_returns_boundless_proving() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert!(matches!(config.proving, ProvingConfig::Boundless(_)));
    }

    #[rstest]
    fn direct_mode_returns_direct_proving() {
        let config = Cli::parse_from(direct_args()).into_config().unwrap();
        assert!(matches!(config.proving, ProvingConfig::Direct { .. }));
    }

    // ── Signing mode variants ───────────────────────────────────────────

    #[rstest]
    fn local_key_returns_local_signing() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert!(matches!(config.signing, SignerConfig::Local { .. }));
    }

    #[rstest]
    fn remote_signer_returns_remote_signing() {
        let config = Cli::parse_from(remote_signer_args()).into_config().unwrap();
        assert!(matches!(config.signing, SignerConfig::Remote { .. }));
    }

    // ── Clap-level validation failures ──────────────────────────────────

    #[rstest]
    fn no_signing_method_succeeds_clap_parse_but_fails_config() {
        let mut args = direct_args();
        args.retain(|a| *a != "--private-key" && *a != TEST_PRIVATE_KEY);
        // The signer macro doesn't require signing args at clap level;
        // the TryFrom conversion catches it.
        if let Ok(cli) = Cli::try_parse_from(args) {
            assert!(cli.into_config().is_err());
        }
    }

    #[rstest]
    fn signer_endpoint_without_address_fails_clap_parse() {
        let mut args = direct_args();
        args.retain(|a| *a != "--private-key" && *a != TEST_PRIVATE_KEY);
        args.extend(["--signer-endpoint", TEST_SIGNER_ENDPOINT]);
        assert!(Cli::try_parse_from(args).is_err());
    }

    // ── into_config validation failures (parametrized) ──────────────────

    #[rstest]
    #[case::zero_poll_interval("--poll-interval-secs", "0")]
    #[case::zero_prover_timeout("--prover-timeout-secs", "0")]
    #[case::zero_boundless_timeout("--boundless-timeout-secs", "0")]
    fn zero_duration_fails_into_config(#[case] flag: &str, #[case] value: &str) {
        let mut args = boundless_args();
        args.extend([flag, value]);
        let result = Cli::try_parse_from(args).expect("clap should parse these args").into_config();
        assert!(result.is_err());
    }

    // ── Field value checks ──────────────────────────────────────────────

    #[rstest]
    fn default_durations() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS));
        assert_eq!(config.prover_timeout, Duration::from_secs(DEFAULT_PROVER_TIMEOUT_SECS));
    }

    #[rstest]
    fn discovery_config_fields() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert_eq!(config.discovery.target_group_arn, TEST_TARGET_GROUP_ARN);
        assert_eq!(config.discovery.aws_region, TEST_AWS_REGION);
        assert_eq!(config.discovery.port, DEFAULT_PROVER_PORT);
    }

    #[rstest]
    fn image_id_parsed_correctly() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        let ProvingConfig::Boundless(b) = &config.proving else {
            panic!("expected Boundless proving config");
        };
        assert_eq!(b.image_id, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[rstest]
    fn tx_manager_config_has_defaults() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert_eq!(config.tx_manager.num_confirmations, 10);
        assert_eq!(config.tx_manager.fee_limit_multiplier, 5);
    }

    // ── parse_image_id unit tests ───────────────────────────────────────

    #[rstest]
    #[case::with_prefix("0x0000000100000002000000030000000400000005000000060000000700000008", [1,2,3,4,5,6,7,8])]
    #[case::without_prefix("0000000100000002000000030000000400000005000000060000000700000008", [1,2,3,4,5,6,7,8])]
    fn parse_image_id_valid(#[case] input: &str, #[case] expected: [u32; 8]) {
        assert_eq!(parse_image_id(input).unwrap(), expected);
    }

    #[rstest]
    #[case::too_short("00000001")]
    #[case::invalid_hex("zzzz")]
    #[case::empty("")]
    fn parse_image_id_invalid(#[case] input: &str) {
        assert!(parse_image_id(input).is_err());
    }
}
