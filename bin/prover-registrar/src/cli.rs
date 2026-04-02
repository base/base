//! CLI argument parsing and config construction for the prover registrar.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_tee_nitro_attestation_prover::{
    AttestationProofProvider, BoundlessProver, DirectProver,
};
use base_proof_tee_registrar::{
    AwsDiscoveryConfig, AwsTargetGroupDiscovery, BoundlessConfig, DEFAULT_MAX_CONCURRENCY,
    DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, DriverConfig, ProverClient, ProvingConfig,
    RegistrarConfig, RegistrarError, RegistrarMetrics, RegistrationDriver, RegistryContractClient,
};
use base_tx_manager::{BaseTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use clap::{Args, Parser, ValueEnum};
use eyre::WrapErr;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

// Generate env-var helper and CLI structs with the `BASE_REGISTRAR_` prefix.
base_cli_utils::define_cli_env!("BASE_REGISTRAR");
base_cli_utils::define_log_args!("BASE_REGISTRAR");
base_cli_utils::define_metrics_args!("BASE_REGISTRAR", 7300);
base_cli_utils::define_health_args!("BASE_REGISTRAR", 8080);
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

    /// Maximum number of instances to process concurrently within a single
    /// registration cycle. Each instance may trigger a ~20-minute proof
    /// generation, so this limits concurrent proof work.
    #[arg(long, env = cli_env!("MAX_CONCURRENCY"), default_value_t = DEFAULT_MAX_CONCURRENCY)]
    max_concurrency: usize,

    // ── Tx Retry ──────────────────────────────────────────────────────────────
    /// Maximum number of transaction submission retries for transient errors.
    #[arg(long, env = cli_env!("MAX_TX_RETRIES"), default_value_t = DEFAULT_MAX_TX_RETRIES)]
    max_tx_retries: u32,

    /// Delay between transaction submission retries, in seconds.
    #[arg(long, env = cli_env!("TX_RETRY_DELAY_SECS"), default_value_t = DEFAULT_TX_RETRY_DELAY_SECS)]
    tx_retry_delay_secs: u64,

    // ── Unhealthy Registration Window ─────────────────────────────────────
    /// Duration (seconds) after EC2 launch during which unhealthy instances
    /// are still eligible for registration. New instances may fail ALB health
    /// checks while the application initializes. Set to 0 to disable.
    #[arg(long, env = cli_env!("UNHEALTHY_REGISTRATION_WINDOW_SECS"), default_value_t = DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS)]
    unhealthy_registration_window_secs: u64,

    // ── Health Server ─────────────────────────────────────────────────────────
    #[command(flatten)]
    health: HealthArgs,

    // ── Logging ───────────────────────────────────────────────────────────────
    #[command(flatten)]
    log: LogArgs,

    // ── Metrics ───────────────────────────────────────────────────────────────
    #[command(flatten)]
    metrics: MetricsArgs,
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

    /// HTTP(S) URL of the Nitro attestation verifier ELF (e.g. Pinata IPFS gateway URL).
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_VERIFIER_PROGRAM_URL"),
        required_if_eq("proving_mode", "boundless")
    )]
    boundless_verifier_program_url: Option<Url>,

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
        id[i] = u32::from_le_bytes(chunk.try_into().unwrap());
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

        if self.max_concurrency == 0 {
            return Err(RegistrarError::Config("--max-concurrency must be greater than 0".into()));
        }

        if self.tx_retry_delay_secs == 0 {
            return Err(RegistrarError::Config(
                "--tx-retry-delay-secs must be greater than 0".into(),
            ));
        }

        if self.health.port == 0 {
            return Err(RegistrarError::Config("health server port must be non-zero".into()));
        }

        let health_addr = self.health.socket_addr();

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
            max_concurrency: self.max_concurrency,
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: Duration::from_secs(self.tx_retry_delay_secs),
            unhealthy_registration_window: Duration::from_secs(
                self.unhealthy_registration_window_secs,
            ),
            health_addr,
        })
    }

    /// Run the registrar service.
    pub(crate) async fn run(mut self) -> eyre::Result<()> {
        // Extract observability args before into_config() consumes self.
        // LogArgs/MetricsArgs are binary-layer concerns, not part of RegistrarConfig.
        let log_config: base_cli_utils::LogConfig = std::mem::take(&mut self.log).into();
        let metrics_config: base_cli_utils::MetricsConfig =
            std::mem::take(&mut self.metrics).into();

        let config = self.into_config()?;

        log_config.init_tracing_subscriber()?;

        // Install the default rustls CryptoProvider before any TLS connections are created.
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Registrar starting");

        // ── 1. Cancellation token and signal handler ─────────────────────────
        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        // ── 2. Metrics recorder (if enabled) ─────────────────────────────────
        metrics_config
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                RegistrarMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;

        // ── 3. Build L1 provider and tx manager ──────────────────────────────
        let provider = RootProvider::new_http(config.l1_rpc_url.clone());

        let tx_manager = SimpleTxManager::new(
            provider,
            config.signing,
            config.tx_manager,
            config.l1_chain_id,
            Arc::new(BaseTxMetrics::new("registrar")),
        )
        .await?;

        // ── 4. Build AWS SDK clients for discovery ───────────────────────────
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

        // ── 5. Build registry client ─────────────────────────────────────────
        let registry = RegistryContractClient::new(
            config.tee_prover_registry_address,
            config.l1_rpc_url.clone(),
        );

        // ── 6. Build proof provider ──────────────────────────────────────────
        let proof_provider: Box<dyn AttestationProofProvider> = match config.proving {
            ProvingConfig::Boundless(ref boundless) => Box::new(BoundlessProver {
                rpc_url: boundless.rpc_url.clone(),
                signer: boundless.signer.clone(),
                verifier_program_url: boundless.verifier_program_url.clone(),
                image_id: boundless.image_id,
                poll_interval: boundless.poll_interval,
                timeout: boundless.timeout,
                trusted_certs_prefix_len: DEFAULT_TRUSTED_CERTS_PREFIX,
                submit_lock: Arc::new(tokio::sync::Mutex::new(())),
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

        // ── 7. Start health HTTP server ──────────────────────────────────────
        // health_handle is awaited during graceful shutdown in step 9 below.
        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = tokio::spawn(HealthServer::serve(
            config.health_addr,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        // ── 8. Build and run driver ──────────────────────────────────────────
        let signer_client = ProverClient::new(config.prover_timeout);
        let driver_config = DriverConfig {
            registry_address: config.tee_prover_registry_address,
            poll_interval: config.poll_interval,
            cancel: cancel.clone(),
            max_concurrency: config.max_concurrency,
            max_tx_retries: config.max_tx_retries,
            tx_retry_delay: config.tx_retry_delay,
            unhealthy_registration_window: config.unhealthy_registration_window,
        };

        // Mark the service as ready. This signals "initialised and running", not
        // "connectivity verified" — the registrar is an outbound-only service that
        // does not receive traffic, so readiness gating on L1/AWS connectivity
        // would add complexity without benefit.
        ready.store(true, Ordering::SeqCst);

        let cancel_guard = cancel.clone().drop_guard();
        let driver_result = RegistrationDriver::new(
            discovery,
            proof_provider,
            registry,
            tx_manager,
            signer_client,
            driver_config,
        )
        .run()
        .await;
        drop(cancel_guard);

        // ── 9. Graceful shutdown (always runs, even on driver error) ─────────
        info!("Driver stopped, shutting down...");
        ready.store(false, Ordering::SeqCst);
        RegistrarMetrics::record_shutdown();

        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        signal_handle.abort();
        match signal_handle.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "Signal handler task panicked"),
        }

        info!("Service stopped");
        driver_result?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

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
    const TEST_VERIFIER_URL: &str = "https://gateway.pinata.cloud/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    const TEST_IMAGE_ID: &str =
        "0x0100000002000000030000000400000005000000060000000700000008000000";
    const TEST_ELF_PATH: &str = "/tmp/guest.elf";
    const TEST_SIGNER_ENDPOINT: &str = "http://localhost:8546";
    const TEST_SIGNER_ADDR: &str = "0x0000000000000000000000000000000000000002";

    const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
    const DEFAULT_PROVER_TIMEOUT_SECS: u64 = 30;
    const DEFAULT_PROVER_PORT: u16 = 8000;
    const DEFAULT_HEALTH_PORT: u16 = 8080;

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
    #[case::zero_max_concurrency("--max-concurrency", "0")]
    #[case::zero_tx_retry_delay("--tx-retry-delay-secs", "0")]
    fn zero_duration_fails_into_config(#[case] flag: &str, #[case] value: &str) {
        let mut args = boundless_args();
        args.extend([flag, value]);
        let result = Cli::try_parse_from(args).expect("clap should parse these args").into_config();
        assert!(result.is_err());
    }

    #[rstest]
    fn health_port_zero_rejected() {
        let mut args = boundless_args();
        args.extend(["--health.port", "0"]);
        let result = Cli::parse_from(args).into_config();
        assert!(result.is_err());
    }

    // ── Field value checks ──────────────────────────────────────────────

    #[rstest]
    fn default_durations_and_concurrency() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS));
        assert_eq!(config.prover_timeout, Duration::from_secs(DEFAULT_PROVER_TIMEOUT_SECS));
        assert_eq!(config.max_concurrency, DEFAULT_MAX_CONCURRENCY);
        assert_eq!(config.max_tx_retries, DEFAULT_MAX_TX_RETRIES);
        assert_eq!(config.tx_retry_delay, Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS));
        assert_eq!(
            config.unhealthy_registration_window,
            Duration::from_secs(DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS),
        );
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

    #[rstest]
    fn default_health_addr() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert_eq!(config.health_addr, SocketAddr::from(([0, 0, 0, 0], DEFAULT_HEALTH_PORT)));
    }

    #[rstest]
    fn custom_health_addr() {
        let mut args = boundless_args();
        args.extend(["--health.addr", "127.0.0.1", "--health.port", "9090"]);
        let config = Cli::parse_from(args).into_config().unwrap();
        assert_eq!(config.health_addr, SocketAddr::from(([127, 0, 0, 1], 9090)));
    }

    #[rstest]
    fn default_metrics_args() {
        let cli = Cli::parse_from(boundless_args());
        assert!(!cli.metrics.enabled);
        assert_eq!(cli.metrics.port, MetricsArgs::default().port);
    }

    #[rstest]
    fn custom_metrics_args() {
        let mut args = boundless_args();
        args.extend(["--metrics.enabled", "--metrics.port", "9100"]);
        let cli = Cli::parse_from(args);
        assert!(cli.metrics.enabled);
        assert_eq!(cli.metrics.port, 9100);
    }

    // ── parse_image_id unit tests ───────────────────────────────────────

    #[rstest]
    #[case::with_prefix("0x0100000002000000030000000400000005000000060000000700000008000000", [1,2,3,4,5,6,7,8])]
    #[case::without_prefix("0100000002000000030000000400000005000000060000000700000008000000", [1,2,3,4,5,6,7,8])]
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
