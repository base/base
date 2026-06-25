//! CLI argument parsing and config construction for the prover registrar.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256};
use alloy_provider::ProviderBuilder;
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_tee_attestation::TeeAttestationProofProvider;
use base_proof_tee_nitro_attestation_prover::{
    BoundlessProver as NitroBoundlessProver, DirectProver as NitroDirectProver,
};
use base_proof_tee_registrar::{
    AwsDiscoveryConfig, AwsTargetGroupDiscovery, BoundlessConfig, CrlConfig,
    DEFAULT_CRL_FETCH_TIMEOUT_SECS, DEFAULT_MAX_ATTESTATION_AGE_SECS, DEFAULT_MAX_CONCURRENCY,
    DEFAULT_MAX_RECOVERY_ATTEMPTS, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, DiscoveryConfig, DriverConfig, InstanceDiscovery,
    PlatformProvingConfig, PlatformRegistrationConfig, ProverClient, ProverFleet, ProvingConfig,
    RegistrarConfig, RegistrarError, RegistrarMetrics, RegistrationDriver, RegistryContractClient,
    SignerAttestationKind, StaticDiscoveryConfig, StaticEndpointDiscovery, TdxBoundlessConfig,
    TdxProvingConfig,
};
use base_proof_tee_tdx_attestation_prover::{
    BoundlessProver as TdxBoundlessProver, DirectProver as TdxDirectProver, RecoveredProofPolicy,
};
use base_proof_tee_tdx_collateral::{DEFAULT_TDX_MAX_QUOTE_AGE_SECS, TdxAttestationConfig};
use base_proof_tee_tdx_verifier::TDXTcbStatus;
use base_tx_manager::{BaseTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use boundless_market::alloy::signers::local::PrivateKeySigner;
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

/// Default JSON-RPC port for prover instances when not explicitly set.
const DEFAULT_PROVER_PORT: u16 = 8000;

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

    // ── Nitro Discovery ──────────────────────────────────────────────────────
    /// Nitro prover discovery mode.
    #[arg(
        long,
        env = cli_env!("NITRO_DISCOVERY_MODE"),
        default_value = "aws-target-group"
    )]
    nitro_discovery_mode: DiscoveryMode,

    /// Nitro AWS ALB target group ARN.
    #[arg(long, env = cli_env!("NITRO_TARGET_GROUP_ARN"))]
    nitro_target_group_arn: Option<String>,

    /// Nitro AWS region.
    #[arg(long, env = cli_env!("NITRO_AWS_REGION"))]
    nitro_aws_region: Option<String>,

    /// Nitro JSON-RPC port for AWS target group discovery.
    #[arg(long, env = cli_env!("NITRO_PROVER_PORT"))]
    nitro_prover_port: Option<u16>,

    /// Nitro prover endpoint for static discovery. Repeat for multiple endpoints.
    #[arg(long, env = cli_env!("NITRO_PROVER_ENDPOINT"))]
    nitro_prover_endpoint: Vec<Url>,

    // ── TDX Discovery ────────────────────────────────────────────────────────
    /// TDX prover discovery mode. TDX is enabled when this or another TDX fleet flag is present.
    #[arg(long, env = cli_env!("TDX_DISCOVERY_MODE"))]
    tdx_discovery_mode: Option<DiscoveryMode>,

    /// TDX AWS ALB target group ARN.
    #[arg(long, env = cli_env!("TDX_TARGET_GROUP_ARN"))]
    tdx_target_group_arn: Option<String>,

    /// TDX AWS region.
    #[arg(long, env = cli_env!("TDX_AWS_REGION"))]
    tdx_aws_region: Option<String>,

    /// TDX JSON-RPC port for AWS target group discovery.
    #[arg(long, env = cli_env!("TDX_PROVER_PORT"), default_value_t = DEFAULT_PROVER_PORT)]
    tdx_prover_port: u16,

    /// TDX prover endpoint for static discovery. Repeat for multiple endpoints.
    #[arg(long, env = cli_env!("TDX_PROVER_ENDPOINT"))]
    tdx_prover_endpoint: Vec<Url>,

    // ── Signing ───────────────────────────────────────────────────────────────
    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    signer: SignerCli,

    // ── Transaction Manager ───────────────────────────────────────────────────
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    tx_manager: TxManagerCli,

    // ── Nitro Proving ────────────────────────────────────────────────────────
    /// Nitro ZK proving backend.
    #[arg(long, env = cli_env!("NITRO_PROVING_MODE"))]
    nitro_proving_mode: Option<ProvingMode>,

    /// Nitro guest program image ID for Boundless mode.
    #[arg(long, env = cli_env!("NITRO_IMAGE_ID"))]
    nitro_image_id: Option<String>,

    /// Nitro guest ELF path for direct mode.
    #[arg(long, env = cli_env!("NITRO_ELF_PATH"))]
    nitro_elf_path: Option<PathBuf>,

    // ── Nitro Boundless ──────────────────────────────────────────────────────
    #[command(flatten)]
    nitro_boundless: NitroBoundlessArgs,

    // ── TDX Proving ──────────────────────────────────────────────────────────
    #[command(flatten)]
    tdx: TdxArgs,

    // ── TDX Collateral / Policy ──────────────────────────────────────────────
    #[command(flatten)]
    tdx_collateral: TdxCollateralArgs,

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

    // ── CRL Checking ───────────────────────────────────────────────────────────
    #[command(flatten)]
    crl: CrlArgs,

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

/// Prover fleet discovery backend selector.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum DiscoveryMode {
    /// Discover instances from an AWS ALB target group.
    AwsTargetGroup,
    /// Poll a configured static endpoint list.
    Static,
}

/// Nitro Boundless Network CLI arguments.
#[derive(Args)]
struct NitroBoundlessArgs {
    /// Boundless Network RPC URL.
    #[arg(long, env = cli_env!("NITRO_BOUNDLESS_RPC_URL"))]
    nitro_boundless_rpc_url: Option<Url>,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long, env = cli_env!("NITRO_BOUNDLESS_PRIVATE_KEY"))]
    nitro_boundless_private_key: Option<String>,

    /// HTTP(S) URL of the Nitro attestation verifier ELF (e.g. Pinata IPFS gateway URL).
    #[arg(long, env = cli_env!("NITRO_BOUNDLESS_VERIFIER_PROGRAM_URL"))]
    nitro_boundless_verifier_program_url: Option<Url>,

    /// Interval between Boundless fulfillment status checks, in seconds.
    #[arg(long, env = cli_env!("NITRO_BOUNDLESS_POLL_INTERVAL_SECS"), default_value_t = 5)]
    nitro_boundless_poll_interval_secs: u64,

    /// Proof generation timeout in seconds.
    #[arg(long, env = cli_env!("NITRO_BOUNDLESS_TIMEOUT_SECS"), default_value_t = 600)]
    nitro_boundless_timeout_secs: u64,

    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    #[arg(
        long,
        env = cli_env!("NITRO_BOUNDLESS_MAX_RECOVERY_ATTEMPTS"),
        default_value_t = DEFAULT_MAX_RECOVERY_ATTEMPTS
    )]
    nitro_boundless_max_recovery_attempts: u32,

    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    #[arg(long, env = cli_env!("NITRO_VERIFIER_ADDRESS"))]
    nitro_verifier_address: Option<Address>,

    /// Maximum age (in seconds) of a recovered proof's attestation timestamp
    /// before it is considered stale. Should be slightly below the on-chain
    /// `MAX_AGE` to account for clock skew. Defaults to 3300 s (55 minutes).
    #[arg(
        long,
        env = cli_env!("NITRO_MAX_ATTESTATION_AGE_SECS"),
        default_value_t = DEFAULT_MAX_ATTESTATION_AGE_SECS
    )]
    nitro_max_attestation_age_secs: u64,
}

/// TDX attestation proving CLI arguments.
#[derive(Args)]
struct TdxArgs {
    /// TDX attestation proving backend.
    #[arg(long, env = cli_env!("TDX_PROVING_MODE"))]
    tdx_proving_mode: Option<TdxProvingMode>,

    /// TDX verifier guest image ID for Boundless mode.
    #[arg(long, env = cli_env!("TDX_IMAGE_ID"))]
    tdx_image_id: Option<String>,

    /// TDX Boundless Network RPC URL.
    #[arg(long, env = cli_env!("TDX_BOUNDLESS_RPC_URL"))]
    tdx_boundless_rpc_url: Option<Url>,

    /// TDX Boundless Network proving fee private key.
    #[arg(long, env = cli_env!("TDX_BOUNDLESS_PRIVATE_KEY"))]
    tdx_boundless_private_key: Option<String>,

    /// HTTP(S) URL of the TDX attestation verifier ELF.
    #[arg(long, env = cli_env!("TDX_BOUNDLESS_VERIFIER_PROGRAM_URL"))]
    tdx_boundless_verifier_program_url: Option<Url>,

    /// Interval between TDX Boundless fulfillment status checks, in seconds.
    #[arg(long, env = cli_env!("TDX_BOUNDLESS_POLL_INTERVAL_SECS"), default_value_t = 5)]
    tdx_boundless_poll_interval_secs: u64,

    /// TDX proof generation timeout in seconds.
    #[arg(long, env = cli_env!("TDX_BOUNDLESS_TIMEOUT_SECS"), default_value_t = 600)]
    tdx_boundless_timeout_secs: u64,

    /// Number of deterministic TDX request-ID slots to probe during recovery.
    #[arg(
        long,
        env = cli_env!("TDX_BOUNDLESS_MAX_RECOVERY_ATTEMPTS"),
        default_value_t = DEFAULT_MAX_RECOVERY_ATTEMPTS
    )]
    tdx_boundless_max_recovery_attempts: u32,

    /// Maximum accepted age of a recovered TDX quote proof, in seconds.
    #[arg(
        long,
        env = cli_env!("TDX_MAX_RECOVERED_QUOTE_AGE_SECS"),
        default_value_t = DEFAULT_TDX_MAX_QUOTE_AGE_SECS
    )]
    tdx_max_recovered_quote_age_secs: u64,
}

/// TDX collateral retrieval and verifier policy CLI arguments.
#[derive(Args)]
struct TdxCollateralArgs {
    /// Intel TDX PCS API base URL.
    #[arg(long, env = cli_env!("TDX_PCS_TDX_BASE_URL"))]
    tdx_pcs_tdx_base_url: Option<Url>,

    /// Trusted Intel SGX/TDX root CA certificate hash.
    #[arg(long, env = cli_env!("TDX_TRUSTED_ROOT_CA_HASH"))]
    tdx_trusted_root_ca_hash: Option<B256>,

    /// Maximum accepted TDX quote age, in seconds.
    #[arg(
        long,
        env = cli_env!("TDX_MAX_QUOTE_AGE_SECS"),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    tdx_max_quote_age_secs: Option<u64>,

    /// Allowed TDX TCB status. Repeat to allow multiple statuses.
    #[arg(long, env = cli_env!("TDX_ALLOWED_TCB_STATUS"), value_enum)]
    tdx_allowed_tcb_status: Vec<TdxTcbStatusArg>,

    /// Intel PCS and CRL fetch timeout, in seconds.
    #[arg(
        long,
        env = cli_env!("TDX_COLLATERAL_FETCH_TIMEOUT_SECS"),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    tdx_collateral_fetch_timeout_secs: Option<u64>,
}

impl TdxCollateralArgs {
    fn config(&self) -> TdxAttestationConfig {
        let mut config = TdxAttestationConfig::intel_pcs();
        if let Some(pcs_tdx_base_url) = &self.tdx_pcs_tdx_base_url {
            config.pcs_tdx_base_url = pcs_tdx_base_url.clone();
        }
        if let Some(trusted_root_ca_hash) = self.tdx_trusted_root_ca_hash {
            config.trusted_root_ca_hash = trusted_root_ca_hash;
        }
        if let Some(max_quote_age_secs) = self.tdx_max_quote_age_secs {
            config.max_quote_age = Duration::from_secs(max_quote_age_secs);
        }
        if !self.tdx_allowed_tcb_status.is_empty() {
            config.allowed_tcb_statuses =
                self.tdx_allowed_tcb_status.iter().map(|status| status.to_contract()).collect();
        }
        if let Some(fetch_timeout_secs) = self.tdx_collateral_fetch_timeout_secs {
            config.fetch_timeout = Duration::from_secs(fetch_timeout_secs);
        }
        config
    }
}

/// CLI representation of contract TDX TCB statuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum TdxTcbStatusArg {
    /// Platform TCB is up to date.
    UpToDate,
    /// Platform needs software hardening.
    SwHardeningNeeded,
    /// Platform needs configuration hardening.
    ConfigurationNeeded,
    /// Platform needs configuration and software hardening.
    ConfigurationAndSwHardeningNeeded,
    /// Platform TCB is out of date.
    OutOfDate,
    /// Platform TCB is out of date and needs configuration hardening.
    OutOfDateConfigurationNeeded,
    /// Platform TCB has been revoked.
    Revoked,
}

impl TdxTcbStatusArg {
    const fn to_contract(self) -> TDXTcbStatus {
        match self {
            Self::UpToDate => TDXTcbStatus::UpToDate,
            Self::SwHardeningNeeded => TDXTcbStatus::SwHardeningNeeded,
            Self::ConfigurationNeeded => TDXTcbStatus::ConfigurationNeeded,
            Self::ConfigurationAndSwHardeningNeeded => {
                TDXTcbStatus::ConfigurationAndSwHardeningNeeded
            }
            Self::OutOfDate => TDXTcbStatus::OutOfDate,
            Self::OutOfDateConfigurationNeeded => TDXTcbStatus::OutOfDateConfigurationNeeded,
            Self::Revoked => TDXTcbStatus::Revoked,
        }
    }
}

/// TDX proving backend selector.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum TdxProvingMode {
    /// Native direct verification for local development and mock contracts.
    Direct,
    /// Boundless marketplace proving for RISC Zero proofs.
    Boundless,
}

/// CRL (Certificate Revocation List) checking CLI arguments.
#[derive(Args)]
struct CrlArgs {
    /// Enable on-demand CRL checking at registration time.
    /// When enabled, intermediate certificates are checked against CRL
    /// distribution points before signer registration. Revoked certificates
    /// trigger a `revokeCert` transaction on-chain.
    #[arg(long, env = cli_env!("CRL_CHECK_ENABLED"), default_value_t = false)]
    crl_check_enabled: bool,

    /// `NitroEnclaveVerifier` contract address for `revokeCert` calls.
    /// Required when `--crl-check-enabled` is set.
    #[arg(long, env = cli_env!("CRL_NITRO_VERIFIER_ADDRESS"))]
    crl_nitro_verifier_address: Option<Address>,

    /// HTTP timeout for CRL fetches from AWS S3 endpoints, in seconds.
    #[arg(
        long,
        env = cli_env!("CRL_FETCH_TIMEOUT_SECS"),
        default_value_t = DEFAULT_CRL_FETCH_TIMEOUT_SECS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    crl_fetch_timeout_secs: u64,
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
fn parse_image_id(field: &str, s: &str) -> std::result::Result<[u32; 8], RegistrarError> {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    let bytes: [u8; 32] = hex::decode(hex)
        .map_err(|e| RegistrarError::Config(format!("{field}: {e}")))?
        .try_into()
        .map_err(|v: Vec<u8>| {
            RegistrarError::Config(format!("{field}: expected 32 bytes, got {}", v.len()))
        })?;

    let mut id = [0u32; 8];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        id[i] = u32::from_le_bytes(chunk.try_into().unwrap());
    }
    Ok(id)
}

fn start_boundless_balance_monitor(
    platform: SignerAttestationKind,
    address: Address,
    rpc_url: Url,
    cancel: CancellationToken,
) {
    let (layer, balance_rx) =
        BalanceMonitorLayer::new(address, cancel, BalanceMonitorLayer::DEFAULT_POLL_INTERVAL);
    let _provider = ProviderBuilder::new().layer(layer).connect_http(rpc_url);
    tokio::spawn(async move {
        let mut rx = balance_rx;
        let platform = platform.rpc_name();
        let address = address.to_string();
        while rx.changed().await.is_ok() {
            RegistrarMetrics::boundless_balance_wei(platform, address.clone())
                .set(f64::from(*rx.borrow_and_update()));
        }
    });
    info!(
        %address,
        platform = platform.rpc_name(),
        "Boundless balance monitor started"
    );
}

async fn build_discovery(config: &DiscoveryConfig) -> eyre::Result<Box<dyn InstanceDiscovery>> {
    match config {
        DiscoveryConfig::AwsTargetGroup(aws) => {
            let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new(aws.aws_region.clone()))
                .load()
                .await;
            let elb_client = aws_sdk_elasticloadbalancingv2::Client::new(&aws_config);
            let ec2_client = aws_sdk_ec2::Client::new(&aws_config);
            Ok(Box::new(AwsTargetGroupDiscovery::new(
                elb_client,
                ec2_client,
                aws.target_group_arn.clone(),
                aws.port,
            )))
        }
        DiscoveryConfig::Static(static_config) => {
            Ok(Box::new(StaticEndpointDiscovery::new(static_config.endpoints.clone())))
        }
    }
}

fn build_proof_provider(
    config: &PlatformProvingConfig,
) -> std::result::Result<Box<dyn TeeAttestationProofProvider>, RegistrarError> {
    match config {
        PlatformProvingConfig::Nitro(ProvingConfig::Boundless(boundless)) => {
            Ok(Box::new(NitroBoundlessProver {
                rpc_url: boundless.rpc_url.clone(),
                signer: boundless.signer.clone(),
                verifier_program_url: boundless.verifier_program_url.clone(),
                image_id: boundless.image_id,
                poll_interval: boundless.poll_interval,
                timeout: boundless.timeout,
                trusted_certs_prefix_len: DEFAULT_TRUSTED_CERTS_PREFIX,
                max_recovery_attempts: boundless.max_recovery_attempts,
                max_attestation_age: boundless.max_attestation_age,
                submit_lock: Arc::new(tokio::sync::Mutex::new(())),
                recovery_blocked: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            }))
        }
        PlatformProvingConfig::Nitro(ProvingConfig::Direct { elf_path }) => {
            let elf = std::fs::read(elf_path).map_err(|e| {
                RegistrarError::Config(format!("failed to read ELF at {}: {e}", elf_path.display()))
            })?;
            let prover =
                NitroDirectProver::new(elf, DEFAULT_TRUSTED_CERTS_PREFIX).map_err(|e| {
                    RegistrarError::Config(format!("failed to create Nitro direct prover: {e}"))
                })?;
            Ok(Box::new(prover))
        }
        PlatformProvingConfig::Tdx(TdxProvingConfig::Direct) => {
            Ok(Box::new(TdxDirectProver::new()))
        }
        PlatformProvingConfig::Tdx(TdxProvingConfig::Boundless(boundless)) => {
            Ok(Box::new(TdxBoundlessProver {
                rpc_url: boundless.rpc_url.clone(),
                signer: boundless.signer.clone(),
                verifier_program_url: boundless.verifier_program_url.clone(),
                image_id: boundless.image_id,
                poll_interval: boundless.poll_interval,
                timeout: boundless.timeout,
                max_recovery_attempts: boundless.max_recovery_attempts,
                recovered_proof_policy: RecoveredProofPolicy::new(
                    boundless.max_recovered_quote_age,
                ),
                submit_lock: Arc::new(tokio::sync::Mutex::new(())),
                recovery_blocked: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            }))
        }
    }
}

impl Cli {
    /// Validate the CLI arguments for logical conflicts and parse into a [`RegistrarConfig`].
    pub(crate) fn into_config(self) -> std::result::Result<RegistrarConfig, RegistrarError> {
        let nitro_discovery = self.build_nitro_discovery_config()?;
        let nitro_proving = self.build_nitro_proving_config()?;
        let mut fleets = vec![PlatformRegistrationConfig {
            attestation_kind: SignerAttestationKind::Nitro,
            discovery: nitro_discovery,
            proving: PlatformProvingConfig::Nitro(nitro_proving),
        }];

        if self.tdx_enabled() {
            fleets.push(PlatformRegistrationConfig {
                attestation_kind: SignerAttestationKind::Tdx,
                discovery: self.build_tdx_discovery_config()?,
                proving: PlatformProvingConfig::Tdx(self.build_tdx_proving_config()?),
            });
        }

        // Convert signing and tx manager config via the macro-generated TryFrom impls.
        let signing = SignerConfig::try_from(self.signer)
            .map_err(|e| RegistrarError::Config(format!("signer: {e}")))?;
        let tx_manager = TxManagerConfig::try_from(self.tx_manager)
            .map_err(|e| RegistrarError::Config(format!("tx-manager: {e}")))?;

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

        // Validate CRL config: if enabled, verifier address is required.
        if self.crl.crl_check_enabled && self.crl.crl_nitro_verifier_address.is_none() {
            return Err(RegistrarError::Config(
                "--crl-nitro-verifier-address is required when --crl-check-enabled is set".into(),
            ));
        }

        let crl = CrlConfig {
            enabled: self.crl.crl_check_enabled,
            nitro_verifier_address: self.crl.crl_nitro_verifier_address,
            fetch_timeout: Duration::from_secs(self.crl.crl_fetch_timeout_secs),
        };
        let tdx_attestation = self.tdx_collateral.config();

        let health_addr = self.health.socket_addr();

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            l1_chain_id: self.l1_chain_id,
            fleets,
            signing,
            tx_manager,
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            prover_timeout: Duration::from_secs(self.prover_timeout_secs),
            max_concurrency: self.max_concurrency,
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: Duration::from_secs(self.tx_retry_delay_secs),
            unhealthy_registration_window: Duration::from_secs(
                self.unhealthy_registration_window_secs,
            ),
            health_addr,
            crl,
            tdx_attestation,
        })
    }

    fn build_nitro_discovery_config(&self) -> std::result::Result<DiscoveryConfig, RegistrarError> {
        match self.nitro_discovery_mode {
            DiscoveryMode::AwsTargetGroup => {
                let target_group_arn = self.nitro_target_group_arn.clone().ok_or_else(|| {
                    RegistrarError::Config(
                        "--nitro-target-group-arn is required for Nitro AWS discovery".into(),
                    )
                })?;
                let aws_region = self.nitro_aws_region.clone().ok_or_else(|| {
                    RegistrarError::Config(
                        "--nitro-aws-region is required for Nitro AWS discovery".into(),
                    )
                })?;
                Ok(DiscoveryConfig::AwsTargetGroup(AwsDiscoveryConfig {
                    target_group_arn,
                    aws_region,
                    port: self.nitro_prover_port.unwrap_or(DEFAULT_PROVER_PORT),
                }))
            }
            DiscoveryMode::Static => {
                if self.nitro_prover_endpoint.is_empty() {
                    return Err(RegistrarError::Config(
                        "--nitro-prover-endpoint is required for Nitro static discovery".into(),
                    ));
                }
                Ok(DiscoveryConfig::Static(StaticDiscoveryConfig {
                    endpoints: self.nitro_prover_endpoint.clone(),
                }))
            }
        }
    }

    fn build_nitro_proving_config(&self) -> std::result::Result<ProvingConfig, RegistrarError> {
        let proving_mode = self
            .nitro_proving_mode
            .ok_or_else(|| RegistrarError::Config("--nitro-proving-mode is required".into()))?;
        match proving_mode {
            ProvingMode::Boundless => {
                if self.nitro_boundless.nitro_boundless_timeout_secs == 0 {
                    return Err(RegistrarError::Config(
                        "--nitro-boundless-timeout-secs must be greater than 0".into(),
                    ));
                }

                let boundless_key = self
                    .nitro_boundless
                    .nitro_boundless_private_key
                    .as_deref()
                    .ok_or_else(|| {
                        RegistrarError::Config("--nitro-boundless-private-key is required".into())
                    })?;
                let image_id_hex = self
                    .nitro_image_id
                    .as_deref()
                    .ok_or_else(|| RegistrarError::Config("--nitro-image-id is required".into()))?;

                Ok(ProvingConfig::Boundless(Box::new(BoundlessConfig {
                    rpc_url: self.nitro_boundless.nitro_boundless_rpc_url.clone().ok_or_else(
                        || RegistrarError::Config("--nitro-boundless-rpc-url is required".into()),
                    )?,
                    signer: parse_private_key("--nitro-boundless-private-key", boundless_key)?,
                    verifier_program_url: self
                        .nitro_boundless
                        .nitro_boundless_verifier_program_url
                        .clone()
                        .ok_or_else(|| {
                            RegistrarError::Config(
                                "--nitro-boundless-verifier-program-url is required".into(),
                            )
                        })?,
                    image_id: parse_image_id("--nitro-image-id", image_id_hex)?,
                    poll_interval: Duration::from_secs(
                        self.nitro_boundless.nitro_boundless_poll_interval_secs,
                    ),
                    timeout: Duration::from_secs(self.nitro_boundless.nitro_boundless_timeout_secs),
                    nitro_verifier_address: self.nitro_boundless.nitro_verifier_address,
                    max_recovery_attempts: self
                        .nitro_boundless
                        .nitro_boundless_max_recovery_attempts,
                    max_attestation_age: Duration::from_secs(
                        self.nitro_boundless.nitro_max_attestation_age_secs,
                    ),
                })))
            }
            ProvingMode::Direct => {
                let elf_path = self.nitro_elf_path.clone().ok_or_else(|| {
                    RegistrarError::Config("--nitro-elf-path is required for direct mode".into())
                })?;
                Ok(ProvingConfig::Direct { elf_path })
            }
        }
    }

    const fn tdx_enabled(&self) -> bool {
        self.tdx_discovery_mode.is_some()
            || self.tdx_target_group_arn.is_some()
            || self.tdx_aws_region.is_some()
            || !self.tdx_prover_endpoint.is_empty()
            || self.tdx.tdx_proving_mode.is_some()
            || self.tdx.tdx_image_id.is_some()
            || self.tdx.tdx_boundless_rpc_url.is_some()
            || self.tdx.tdx_boundless_private_key.is_some()
            || self.tdx.tdx_boundless_verifier_program_url.is_some()
    }

    fn build_tdx_discovery_config(&self) -> std::result::Result<DiscoveryConfig, RegistrarError> {
        let mode = self.tdx_discovery_mode.unwrap_or_else(|| {
            if self.tdx_target_group_arn.is_some() || self.tdx_aws_region.is_some() {
                DiscoveryMode::AwsTargetGroup
            } else {
                DiscoveryMode::Static
            }
        });
        match mode {
            DiscoveryMode::AwsTargetGroup => {
                let target_group_arn = self.tdx_target_group_arn.clone().ok_or_else(|| {
                    RegistrarError::Config(
                        "--tdx-target-group-arn is required for TDX AWS discovery".into(),
                    )
                })?;
                let aws_region = self.tdx_aws_region.clone().ok_or_else(|| {
                    RegistrarError::Config(
                        "--tdx-aws-region is required for TDX AWS discovery".into(),
                    )
                })?;
                Ok(DiscoveryConfig::AwsTargetGroup(AwsDiscoveryConfig {
                    target_group_arn,
                    aws_region,
                    port: self.tdx_prover_port,
                }))
            }
            DiscoveryMode::Static => {
                if self.tdx_prover_endpoint.is_empty() {
                    return Err(RegistrarError::Config(
                        "--tdx-prover-endpoint is required for TDX static discovery".into(),
                    ));
                }
                Ok(DiscoveryConfig::Static(StaticDiscoveryConfig {
                    endpoints: self.tdx_prover_endpoint.clone(),
                }))
            }
        }
    }

    fn build_tdx_proving_config(&self) -> std::result::Result<TdxProvingConfig, RegistrarError> {
        let mode = self.tdx.tdx_proving_mode.ok_or_else(|| {
            RegistrarError::Config("--tdx-proving-mode is required when TDX is enabled".into())
        })?;
        match mode {
            TdxProvingMode::Direct => Ok(TdxProvingConfig::Direct),
            TdxProvingMode::Boundless => {
                if self.tdx.tdx_boundless_timeout_secs == 0 {
                    return Err(RegistrarError::Config(
                        "--tdx-boundless-timeout-secs must be greater than 0".into(),
                    ));
                }
                let boundless_key =
                    self.tdx.tdx_boundless_private_key.as_deref().ok_or_else(|| {
                        RegistrarError::Config("--tdx-boundless-private-key is required".into())
                    })?;
                let image_id_hex = self.tdx.tdx_image_id.as_deref().ok_or_else(|| {
                    RegistrarError::Config("--tdx-image-id is required for TDX Boundless".into())
                })?;
                Ok(TdxProvingConfig::Boundless(Box::new(TdxBoundlessConfig {
                    rpc_url: self.tdx.tdx_boundless_rpc_url.clone().ok_or_else(|| {
                        RegistrarError::Config("--tdx-boundless-rpc-url is required".into())
                    })?,
                    signer: parse_private_key("--tdx-boundless-private-key", boundless_key)?,
                    verifier_program_url: self
                        .tdx
                        .tdx_boundless_verifier_program_url
                        .clone()
                        .ok_or_else(|| {
                            RegistrarError::Config(
                                "--tdx-boundless-verifier-program-url is required".into(),
                            )
                        })?,
                    image_id: parse_image_id("--tdx-image-id", image_id_hex)?,
                    poll_interval: Duration::from_secs(self.tdx.tdx_boundless_poll_interval_secs),
                    timeout: Duration::from_secs(self.tdx.tdx_boundless_timeout_secs),
                    max_recovery_attempts: self.tdx.tdx_boundless_max_recovery_attempts,
                    max_recovered_quote_age: Duration::from_secs(
                        self.tdx.tdx_max_recovered_quote_age_secs,
                    ),
                })))
            }
        }
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

        // ── Cancellation token and signal handler ────────────────────────────
        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        // ── Metrics recorder (if enabled) ────────────────────────────────────
        let metrics_enabled = metrics_config.enabled;
        metrics_config
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                RegistrarMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;

        // ── Build L1 provider and tx manager ─────────────────────────────────
        let l1_addr = config.signing.address();
        let provider = if metrics_enabled {
            let (layer, balance_rx) = BalanceMonitorLayer::new(
                l1_addr,
                cancel.clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            let provider =
                ProviderBuilder::new().layer(layer).connect_http(config.l1_rpc_url.clone());
            tokio::spawn(async move {
                let mut rx = balance_rx;
                while rx.changed().await.is_ok() {
                    RegistrarMetrics::account_balance_wei().set(f64::from(*rx.borrow_and_update()));
                }
            });
            info!(%l1_addr, "L1 balance monitor started");

            for fleet in &config.fleets {
                let boundless = match &fleet.proving {
                    PlatformProvingConfig::Nitro(ProvingConfig::Boundless(b)) => {
                        Some((SignerAttestationKind::Nitro, b.signer.address(), &b.rpc_url))
                    }
                    PlatformProvingConfig::Tdx(TdxProvingConfig::Boundless(b)) => {
                        Some((SignerAttestationKind::Tdx, b.signer.address(), &b.rpc_url))
                    }
                    PlatformProvingConfig::Nitro(ProvingConfig::Direct { .. })
                    | PlatformProvingConfig::Tdx(TdxProvingConfig::Direct) => None,
                };
                if let Some((kind, addr, rpc)) = boundless {
                    start_boundless_balance_monitor(kind, addr, rpc.clone(), cancel.clone());
                }
            }

            provider
        } else {
            ProviderBuilder::new().connect_http(config.l1_rpc_url.clone())
        };

        let tx_manager = SimpleTxManager::new(
            provider,
            config.signing,
            config.tx_manager,
            config.l1_chain_id,
            Arc::new(BaseTxMetrics::new("registrar")),
        )
        .await?;

        // ── Build platform fleets ────────────────────────────────────────────
        let mut fleets = Vec::with_capacity(config.fleets.len());
        for fleet_config in &config.fleets {
            let discovery = build_discovery(&fleet_config.discovery).await?;
            let proof_provider = build_proof_provider(&fleet_config.proving)?;
            fleets.push(ProverFleet::new(fleet_config.attestation_kind, discovery, proof_provider));
        }

        // ── Build registry client ────────────────────────────────────────────
        let registry = RegistryContractClient::new(
            config.tee_prover_registry_address,
            config.l1_rpc_url.clone(),
        );

        // ── Start health HTTP server ─────────────────────────────────────────
        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = tokio::spawn(HealthServer::serve(
            config.health_addr,
            Arc::clone(&ready),
            cancel.clone(),
        ));

        // ── Build and run driver ─────────────────────────────────────────────
        let signer_client = ProverClient::new(config.prover_timeout);
        let driver_config = DriverConfig {
            registry_address: config.tee_prover_registry_address,
            poll_interval: config.poll_interval,
            cancel: cancel.clone(),
            max_concurrency: config.max_concurrency,
            max_tx_retries: config.max_tx_retries,
            tx_retry_delay: config.tx_retry_delay,
            unhealthy_registration_window: config.unhealthy_registration_window,
            crl: config.crl,
            tdx_attestation: config.tdx_attestation,
        };

        // Mark the service as ready. This signals "initialised and running", not
        // "connectivity verified" — the registrar is an outbound-only service that
        // does not receive traffic, so readiness gating on L1/AWS connectivity
        // would add complexity without benefit.
        ready.store(true, Ordering::SeqCst);

        let cancel_guard = cancel.clone().drop_guard();
        let driver_result = RegistrationDriver::new_with_fleets(
            fleets,
            registry,
            tx_manager,
            signer_client,
            driver_config,
        )
        .run()
        .await;
        drop(cancel_guard);

        // ── Graceful shutdown (always runs, even on driver error) ────────────
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

    use clap::{CommandFactory, error::ErrorKind};
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
    const TEST_TDX_ENDPOINT: &str = "http://127.0.0.1:9000";
    const TEST_TDX_TARGET_GROUP_ARN: &str =
        "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/tdx/def456";
    const TEST_SIGNER_ENDPOINT: &str = "http://localhost:8546";
    const TEST_SIGNER_ADDR: &str = "0x0000000000000000000000000000000000000002";

    const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
    const DEFAULT_PROVER_TIMEOUT_SECS: u64 = 30;
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
            "--nitro-target-group-arn",
            TEST_TARGET_GROUP_ARN,
            "--nitro-aws-region",
            TEST_AWS_REGION,
            "--private-key",
            TEST_PRIVATE_KEY,
        ]
    }

    /// Boundless-mode args: common + boundless proving.
    fn boundless_args() -> Vec<&'static str> {
        let mut args = common_args();
        args.extend([
            "--nitro-proving-mode",
            "boundless",
            "--nitro-image-id",
            TEST_IMAGE_ID,
            "--nitro-boundless-rpc-url",
            TEST_BOUNDLESS_RPC,
            "--nitro-boundless-private-key",
            TEST_BOUNDLESS_KEY,
            "--nitro-boundless-verifier-program-url",
            TEST_VERIFIER_URL,
        ]);
        args
    }

    /// Direct-mode args: common + direct proving.
    fn direct_args() -> Vec<&'static str> {
        let mut args = common_args();
        args.extend(["--nitro-proving-mode", "direct", "--nitro-elf-path", TEST_ELF_PATH]);
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
            "--nitro-target-group-arn",
            TEST_TARGET_GROUP_ARN,
            "--nitro-aws-region",
            TEST_AWS_REGION,
            "--signer-endpoint",
            TEST_SIGNER_ENDPOINT,
            "--signer-address",
            TEST_SIGNER_ADDR,
            "--nitro-proving-mode",
            "boundless",
            "--nitro-image-id",
            TEST_IMAGE_ID,
            "--nitro-boundless-rpc-url",
            TEST_BOUNDLESS_RPC,
            "--nitro-boundless-private-key",
            TEST_BOUNDLESS_KEY,
            "--nitro-boundless-verifier-program-url",
            TEST_VERIFIER_URL,
        ]
    }

    fn nitro_fleet(config: &RegistrarConfig) -> &PlatformRegistrationConfig {
        config
            .fleets
            .iter()
            .find(|fleet| fleet.attestation_kind == SignerAttestationKind::Nitro)
            .expect("Nitro fleet should be configured")
    }

    fn nitro_discovery(config: &RegistrarConfig) -> &AwsDiscoveryConfig {
        let DiscoveryConfig::AwsTargetGroup(discovery) = &nitro_fleet(config).discovery else {
            panic!("expected Nitro AWS discovery config");
        };
        discovery
    }

    fn nitro_proving(config: &RegistrarConfig) -> &ProvingConfig {
        let PlatformProvingConfig::Nitro(proving) = &nitro_fleet(config).proving else {
            panic!("expected Nitro proving config");
        };
        proving
    }

    fn tdx_fleet(config: &RegistrarConfig) -> &PlatformRegistrationConfig {
        config
            .fleets
            .iter()
            .find(|fleet| fleet.attestation_kind == SignerAttestationKind::Tdx)
            .expect("TDX fleet should be configured")
    }

    // ── Happy-path parsing ──────────────────────────────────────────────

    #[rstest]
    #[case::boundless(boundless_args())]
    #[case::direct(direct_args())]
    #[case::remote_signer(remote_signer_args())]
    fn valid_config_parses(#[case] args: Vec<&str>) {
        assert!(Cli::parse_from(args).into_config().is_ok());
    }

    #[rstest]
    fn dual_nitro_tdx_static_config_parses() {
        let mut args = boundless_args();
        args.extend([
            "--tdx-discovery-mode",
            "static",
            "--tdx-prover-endpoint",
            TEST_TDX_ENDPOINT,
            "--tdx-proving-mode",
            "direct",
        ]);

        let config = Cli::parse_from(args).into_config().unwrap();

        assert_eq!(config.fleets.len(), 2);
        let DiscoveryConfig::Static(static_config) = &tdx_fleet(&config).discovery else {
            panic!("expected TDX static discovery");
        };
        assert_eq!(static_config.endpoints, vec![Url::parse(TEST_TDX_ENDPOINT).unwrap()]);
        assert!(matches!(
            tdx_fleet(&config).proving,
            PlatformProvingConfig::Tdx(TdxProvingConfig::Direct)
        ));
    }

    #[rstest]
    fn nitro_static_discovery_config_parses() {
        let mut args = direct_args();
        args.retain(|arg| {
            !matches!(
                *arg,
                "--nitro-target-group-arn"
                    | TEST_TARGET_GROUP_ARN
                    | "--nitro-aws-region"
                    | TEST_AWS_REGION
            )
        });
        args.extend([
            "--nitro-discovery-mode",
            "static",
            "--nitro-prover-endpoint",
            "http://127.0.0.1:8000",
        ]);

        let config = Cli::parse_from(args).into_config().unwrap();

        assert!(matches!(nitro_fleet(&config).discovery, DiscoveryConfig::Static(_)));
    }

    #[rstest]
    fn tdx_aws_discovery_config_parses() {
        let mut args = boundless_args();
        args.extend([
            "--tdx-discovery-mode",
            "aws-target-group",
            "--tdx-target-group-arn",
            TEST_TDX_TARGET_GROUP_ARN,
            "--tdx-aws-region",
            TEST_AWS_REGION,
            "--tdx-prover-port",
            "9000",
            "--tdx-proving-mode",
            "direct",
        ]);

        let config = Cli::parse_from(args).into_config().unwrap();

        let DiscoveryConfig::AwsTargetGroup(discovery) = &tdx_fleet(&config).discovery else {
            panic!("expected TDX AWS discovery");
        };
        assert_eq!(discovery.target_group_arn, TEST_TDX_TARGET_GROUP_ARN);
        assert_eq!(discovery.aws_region, TEST_AWS_REGION);
        assert_eq!(discovery.port, 9000);
    }

    #[rstest]
    fn tdx_aws_discovery_mode_is_inferred_from_aws_flags() {
        let mut args = boundless_args();
        args.extend([
            "--tdx-target-group-arn",
            TEST_TDX_TARGET_GROUP_ARN,
            "--tdx-aws-region",
            TEST_AWS_REGION,
            "--tdx-prover-port",
            "9000",
            "--tdx-proving-mode",
            "direct",
        ]);

        let config = Cli::parse_from(args).into_config().unwrap();

        let DiscoveryConfig::AwsTargetGroup(discovery) = &tdx_fleet(&config).discovery else {
            panic!("expected TDX AWS discovery");
        };
        assert_eq!(discovery.target_group_arn, TEST_TDX_TARGET_GROUP_ARN);
        assert_eq!(discovery.aws_region, TEST_AWS_REGION);
        assert_eq!(discovery.port, 9000);
    }

    #[rstest]
    fn tdx_direct_config_parses_without_coprocessor_flag() {
        let mut args = boundless_args();
        args.extend(["--tdx-prover-endpoint", TEST_TDX_ENDPOINT, "--tdx-proving-mode", "direct"]);

        let config = Cli::parse_from(args).into_config().unwrap();

        assert!(matches!(
            &tdx_fleet(&config).proving,
            PlatformProvingConfig::Tdx(TdxProvingConfig::Direct)
        ));
    }

    #[rstest]
    fn tdx_collateral_policy_config_parses() {
        let root_hash = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let pcs_url = "https://pcs.example.test/tdx/certification/v4/";
        let mut args = boundless_args();
        args.extend([
            "--tdx-prover-endpoint",
            TEST_TDX_ENDPOINT,
            "--tdx-proving-mode",
            "direct",
            "--tdx-pcs-tdx-base-url",
            pcs_url,
            "--tdx-trusted-root-ca-hash",
            root_hash,
            "--tdx-max-quote-age-secs",
            "120",
            "--tdx-allowed-tcb-status",
            "up-to-date",
            "--tdx-allowed-tcb-status",
            "sw-hardening-needed",
            "--tdx-collateral-fetch-timeout-secs",
            "7",
        ]);

        let config = Cli::parse_from(args).into_config().unwrap();

        assert_eq!(config.tdx_attestation.pcs_tdx_base_url, Url::parse(pcs_url).unwrap());
        assert_eq!(config.tdx_attestation.trusted_root_ca_hash, root_hash.parse::<B256>().unwrap());
        assert_eq!(config.tdx_attestation.max_quote_age, Duration::from_secs(120));
        assert_eq!(
            config
                .tdx_attestation
                .allowed_tcb_statuses
                .iter()
                .map(|status| *status as u8)
                .collect::<Vec<_>>(),
            vec![TDXTcbStatus::UpToDate as u8, TDXTcbStatus::SwHardeningNeeded as u8]
        );
        assert_eq!(config.tdx_attestation.fetch_timeout, Duration::from_secs(7));
    }

    #[rstest]
    fn tdx_collateral_defaults_do_not_enable_tdx_fleet() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();

        assert_eq!(config.fleets.len(), 1);
        assert_eq!(
            config.tdx_attestation.max_quote_age,
            Duration::from_secs(DEFAULT_TDX_MAX_QUOTE_AGE_SECS),
        );
        assert_eq!(
            config
                .tdx_attestation
                .allowed_tcb_statuses
                .iter()
                .map(|status| *status as u8)
                .collect::<Vec<_>>(),
            vec![TDXTcbStatus::UpToDate as u8]
        );
    }

    #[rstest]
    fn nitro_prefixed_args_parse() {
        let mut args = vec![
            "prover-registrar",
            "--l1-rpc-url",
            TEST_L1_RPC,
            "--l1-chain-id",
            TEST_L1_CHAIN_ID,
            "--tee-prover-registry-address",
            TEST_REGISTRY_ADDR,
            "--nitro-target-group-arn",
            TEST_TARGET_GROUP_ARN,
            "--nitro-aws-region",
            TEST_AWS_REGION,
            "--private-key",
            TEST_PRIVATE_KEY,
            "--nitro-proving-mode",
            "direct",
            "--nitro-elf-path",
            TEST_ELF_PATH,
        ];
        args.extend(["--nitro-prover-port", "8100"]);

        let config = Cli::parse_from(args).into_config().unwrap();

        assert_eq!(nitro_discovery(&config).port, 8100);
        assert!(matches!(nitro_proving(&config), ProvingConfig::Direct { .. }));
    }

    #[rstest]
    #[case::target_group_arn("--target-group-arn")]
    #[case::aws_region("--aws-region")]
    #[case::prover_port("--prover-port")]
    #[case::proving_mode("--proving-mode")]
    #[case::image_id("--image-id")]
    #[case::elf_path("--elf-path")]
    #[case::boundless_rpc_url("--boundless-rpc-url")]
    #[case::boundless_private_key("--boundless-private-key")]
    #[case::boundless_verifier_program_url("--boundless-verifier-program-url")]
    #[case::boundless_poll_interval_secs("--boundless-poll-interval-secs")]
    #[case::boundless_timeout_secs("--boundless-timeout-secs")]
    #[case::boundless_max_recovery_attempts("--boundless-max-recovery-attempts")]
    #[case::max_attestation_age_secs("--max-attestation-age-secs")]
    fn legacy_nitro_alias_flags_are_rejected(#[case] flag: &str) {
        let Err(error) = Cli::try_parse_from(["prover-registrar", flag, "value"]) else {
            panic!("legacy Nitro alias flag should be rejected");
        };

        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }

    #[rstest]
    fn help_lists_separate_nitro_and_tdx_configuration() {
        let help = Cli::command().render_long_help().to_string();

        for flag in [
            "--nitro-discovery-mode",
            "--nitro-target-group-arn",
            "--nitro-aws-region",
            "--nitro-prover-port",
            "--nitro-prover-endpoint",
            "--nitro-proving-mode",
            "--nitro-image-id",
            "--nitro-elf-path",
            "--nitro-boundless-rpc-url",
            "--nitro-boundless-private-key",
            "--nitro-boundless-verifier-program-url",
            "--nitro-boundless-poll-interval-secs",
            "--nitro-boundless-timeout-secs",
            "--nitro-boundless-max-recovery-attempts",
            "--nitro-max-attestation-age-secs",
            "--tdx-discovery-mode",
            "--tdx-target-group-arn",
            "--tdx-prover-endpoint",
            "--tdx-proving-mode",
            "--tdx-pcs-tdx-base-url",
            "--tdx-trusted-root-ca-hash",
            "--tdx-allowed-tcb-status",
            "--tdx-collateral-fetch-timeout-secs",
        ] {
            assert!(help.contains(flag), "help should contain {flag}");
        }
        for flag in [
            "--target-group-arn",
            "--aws-region",
            "--prover-port",
            "--proving-mode",
            "--image-id",
            "--elf-path",
            "--boundless-rpc-url",
            "--boundless-private-key",
            "--boundless-verifier-program-url",
            "--boundless-poll-interval-secs",
            "--boundless-timeout-secs",
            "--boundless-max-recovery-attempts",
            "--max-attestation-age-secs",
        ] {
            assert!(!help.contains(flag), "help should not contain legacy alias {flag}");
        }
        assert!(
            !help.contains("--tee-platform"),
            "TDX should not be presented as a mutually exclusive platform mode"
        );
    }

    #[rstest]
    fn env_lists_separate_nitro_and_tdx_configuration() {
        let command = Cli::command();
        let env_vars = command
            .get_arguments()
            .filter_map(|arg| arg.get_env().map(|env| env.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();

        for env in [
            "BASE_REGISTRAR_NITRO_TARGET_GROUP_ARN",
            "BASE_REGISTRAR_NITRO_AWS_REGION",
            "BASE_REGISTRAR_NITRO_PROVER_PORT",
            "BASE_REGISTRAR_NITRO_PROVING_MODE",
            "BASE_REGISTRAR_NITRO_IMAGE_ID",
            "BASE_REGISTRAR_NITRO_ELF_PATH",
            "BASE_REGISTRAR_NITRO_BOUNDLESS_RPC_URL",
            "BASE_REGISTRAR_NITRO_BOUNDLESS_PRIVATE_KEY",
            "BASE_REGISTRAR_NITRO_BOUNDLESS_VERIFIER_PROGRAM_URL",
            "BASE_REGISTRAR_NITRO_BOUNDLESS_POLL_INTERVAL_SECS",
            "BASE_REGISTRAR_NITRO_BOUNDLESS_TIMEOUT_SECS",
            "BASE_REGISTRAR_NITRO_BOUNDLESS_MAX_RECOVERY_ATTEMPTS",
            "BASE_REGISTRAR_NITRO_MAX_ATTESTATION_AGE_SECS",
            "BASE_REGISTRAR_TDX_TARGET_GROUP_ARN",
            "BASE_REGISTRAR_TDX_AWS_REGION",
            "BASE_REGISTRAR_TDX_PROVER_PORT",
            "BASE_REGISTRAR_TDX_PROVING_MODE",
            "BASE_REGISTRAR_TDX_IMAGE_ID",
            "BASE_REGISTRAR_TDX_BOUNDLESS_RPC_URL",
        ] {
            assert!(env_vars.iter().any(|actual| actual == env), "env should contain {env}");
        }

        for env in [
            "BASE_REGISTRAR_TARGET_GROUP_ARN",
            "BASE_REGISTRAR_AWS_REGION",
            "BASE_REGISTRAR_PROVER_PORT",
            "BASE_REGISTRAR_PROVING_MODE",
            "BASE_REGISTRAR_IMAGE_ID",
            "BASE_REGISTRAR_ELF_PATH",
            "BASE_REGISTRAR_BOUNDLESS_RPC_URL",
            "BASE_REGISTRAR_BOUNDLESS_PRIVATE_KEY",
            "BASE_REGISTRAR_BOUNDLESS_VERIFIER_PROGRAM_URL",
            "BASE_REGISTRAR_BOUNDLESS_POLL_INTERVAL_SECS",
            "BASE_REGISTRAR_BOUNDLESS_TIMEOUT_SECS",
            "BASE_REGISTRAR_BOUNDLESS_MAX_RECOVERY_ATTEMPTS",
            "BASE_REGISTRAR_MAX_ATTESTATION_AGE_SECS",
        ] {
            assert!(
                !env_vars.iter().any(|actual| actual == env),
                "env should not contain legacy alias {env}"
            );
        }
    }

    // ── Proving mode variants ───────────────────────────────────────────

    #[rstest]
    fn boundless_mode_returns_boundless_proving() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert!(matches!(nitro_proving(&config), ProvingConfig::Boundless(_)));
    }

    #[rstest]
    fn direct_mode_returns_direct_proving() {
        let config = Cli::parse_from(direct_args()).into_config().unwrap();
        assert!(matches!(nitro_proving(&config), ProvingConfig::Direct { .. }));
    }

    #[rstest]
    fn tdx_boundless_recovered_quote_age_defaults_to_tdx_freshness_window() {
        let mut args = boundless_args();
        args.extend([
            "--tdx-prover-endpoint",
            TEST_TDX_ENDPOINT,
            "--tdx-proving-mode",
            "boundless",
            "--tdx-image-id",
            TEST_IMAGE_ID,
            "--tdx-boundless-rpc-url",
            TEST_BOUNDLESS_RPC,
            "--tdx-boundless-private-key",
            TEST_BOUNDLESS_KEY,
            "--tdx-boundless-verifier-program-url",
            TEST_VERIFIER_URL,
        ]);

        let config = Cli::parse_from(args).into_config().unwrap();
        let PlatformProvingConfig::Tdx(TdxProvingConfig::Boundless(boundless)) =
            &tdx_fleet(&config).proving
        else {
            panic!("expected TDX Boundless proving");
        };

        assert_eq!(
            boundless.max_recovered_quote_age,
            Duration::from_secs(DEFAULT_TDX_MAX_QUOTE_AGE_SECS),
        );
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
    #[case::zero_boundless_timeout("--nitro-boundless-timeout-secs", "0")]
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
        let discovery = nitro_discovery(&config);
        assert_eq!(discovery.target_group_arn, TEST_TARGET_GROUP_ARN);
        assert_eq!(discovery.aws_region, TEST_AWS_REGION);
        assert_eq!(discovery.port, DEFAULT_PROVER_PORT);
    }

    #[rstest]
    fn image_id_parsed_correctly() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        let ProvingConfig::Boundless(b) = nitro_proving(&config) else {
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
        assert_eq!(parse_image_id("--nitro-image-id", input).unwrap(), expected);
    }

    #[rstest]
    #[case::too_short("00000001")]
    #[case::invalid_hex("zzzz")]
    #[case::empty("")]
    fn parse_image_id_invalid(#[case] input: &str) {
        assert!(parse_image_id("--nitro-image-id", input).is_err());
    }

    // ── CRL config validation tests ─────────────────────────────────────

    /// A test address for `--crl-nitro-verifier-address`.
    const TEST_CRL_VERIFIER_ADDR: &str = "0x0000000000000000000000000000000000000099";

    #[rstest]
    fn crl_enabled_without_verifier_address_fails() {
        let mut args = boundless_args();
        args.extend(["--crl-check-enabled"]);
        let result = Cli::parse_from(args).into_config();
        assert!(result.is_err(), "CRL enabled without --crl-nitro-verifier-address should fail");
    }

    #[rstest]
    fn crl_enabled_with_zero_timeout_fails() {
        let mut args = boundless_args();
        args.extend([
            "--crl-check-enabled",
            "--crl-nitro-verifier-address",
            TEST_CRL_VERIFIER_ADDR,
            "--crl-fetch-timeout-secs",
            "0",
        ]);
        let result = Cli::try_parse_from(args);
        assert!(result.is_err(), "--crl-fetch-timeout-secs 0 should be rejected by clap");
    }

    #[rstest]
    fn crl_enabled_with_valid_config_parses() {
        let mut args = boundless_args();
        args.extend([
            "--crl-check-enabled",
            "--crl-nitro-verifier-address",
            TEST_CRL_VERIFIER_ADDR,
        ]);
        let config = Cli::parse_from(args).into_config().unwrap();
        assert!(config.crl.enabled);
        assert!(config.crl.nitro_verifier_address.is_some());
        assert_eq!(config.crl.fetch_timeout, Duration::from_secs(DEFAULT_CRL_FETCH_TIMEOUT_SECS));
    }

    #[rstest]
    fn crl_disabled_by_default() {
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert!(!config.crl.enabled);
        assert!(config.crl.nitro_verifier_address.is_none());
    }

    #[rstest]
    fn crl_disabled_allows_missing_verifier_address() {
        // When CRL is disabled (default), not providing
        // --crl-nitro-verifier-address should be fine.
        let config = Cli::parse_from(boundless_args()).into_config().unwrap();
        assert!(!config.crl.enabled);
    }
}
