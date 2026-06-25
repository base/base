use std::{net::SocketAddr, path::PathBuf, time::Duration};

use alloy_primitives::Address;
use base_proof_tee_tdx_collateral::TdxAttestationConfig;
use base_tx_manager::{SignerConfig, TxManagerConfig};
use boundless_market::alloy::signers::local::PrivateKeySigner;
use url::Url;

use crate::SignerAttestationKind;

/// AWS ALB target group discovery configuration.
///
/// Contains the parameters needed to construct an [`AwsTargetGroupDiscovery`]
/// at runtime. The SDK clients are built separately from these values.
///
/// [`AwsTargetGroupDiscovery`]: crate::AwsTargetGroupDiscovery
#[derive(Clone, Debug)]
pub struct AwsDiscoveryConfig {
    /// AWS ALB target group ARN for prover instance discovery.
    pub target_group_arn: String,
    /// AWS region (e.g. `"us-east-1"`).
    pub aws_region: String,
    /// JSON-RPC port to poll on each prover instance.
    pub port: u16,
}

/// Static endpoint discovery configuration.
#[derive(Clone, Debug)]
pub struct StaticDiscoveryConfig {
    /// JSON-RPC endpoints to poll directly.
    pub endpoints: Vec<Url>,
}

/// Per-fleet discovery configuration.
#[derive(Clone, Debug)]
pub enum DiscoveryConfig {
    /// Discover prover instances through an AWS target group.
    AwsTargetGroup(AwsDiscoveryConfig),
    /// Poll a fixed list of prover JSON-RPC endpoints.
    Static(StaticDiscoveryConfig),
}

/// Default number of deterministic request-ID slots to probe when
/// recovering in-flight Boundless proofs after an instance rotation.
pub const DEFAULT_MAX_RECOVERY_ATTEMPTS: u32 = 5;

/// Default maximum age (in seconds) of a recovered proof's attestation
/// timestamp before it is considered stale and skipped.
///
/// Set to 3300 s (55 minutes), slightly under the on-chain `MAX_AGE` of
/// 60 minutes, to account for clock skew and processing delays.
pub const DEFAULT_MAX_ATTESTATION_AGE_SECS: u64 = 3300;

/// Boundless Network configuration for ZK proof generation.
#[derive(Clone)]
pub struct BoundlessConfig {
    /// Boundless Network RPC URL.
    pub rpc_url: Url,
    /// Signer for Boundless Network proving fees.
    pub signer: PrivateKeySigner,
    /// HTTP(S) URL of the Nitro attestation verifier ELF uploaded via `nitro-attest-cli`
    /// (e.g. a Pinata or Boundless IPFS gateway URL).
    pub verifier_program_url: Url,
    /// Expected image ID of the guest program (hex-encoded `[u32; 8]`).
    pub image_id: [u32; 8],
    /// Interval between fulfillment status checks.
    pub poll_interval: Duration,
    /// Proof generation timeout.
    pub timeout: Duration,
    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    pub nitro_verifier_address: Option<Address>,
    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    pub max_recovery_attempts: u32,
    /// Maximum age of a recovered proof's attestation timestamp before it
    /// is considered stale and skipped. Should be set slightly below the
    /// on-chain `MAX_AGE` to account for clock skew.
    pub max_attestation_age: Duration,
}

impl std::fmt::Debug for BoundlessConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundlessConfig")
            .field("rpc_url", &url_origin(&self.rpc_url))
            .field("signer", &self.signer.address())
            .field("verifier_program_url", &url_origin(&self.verifier_program_url))
            .field("image_id", &self.image_id)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("nitro_verifier_address", &self.nitro_verifier_address)
            .field("max_recovery_attempts", &self.max_recovery_attempts)
            .field("max_attestation_age", &self.max_attestation_age)
            .finish()
    }
}

/// ZK proving backend configuration.
#[derive(Clone, Debug)]
pub enum ProvingConfig {
    /// Boundless marketplace proving (production).
    Boundless(Box<BoundlessConfig>),
    /// Direct proving via `risc0_zkvm::default_prover()` (Bonsai remote or dev-mode).
    Direct {
        /// Path to the guest ELF binary on disk.
        elf_path: PathBuf,
    },
}

/// TDX attestation proving backend configuration.
#[derive(Clone)]
pub enum TdxProvingConfig {
    /// Native direct verification for local development and mock contracts.
    Direct,
    /// RISC Zero proving through the Boundless marketplace.
    Boundless(Box<TdxBoundlessConfig>),
}

impl std::fmt::Debug for TdxProvingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => f.write_str("Direct"),
            Self::Boundless(config) => f.debug_tuple("Boundless").field(config).finish(),
        }
    }
}

/// Boundless Network configuration for TDX attestation proof generation.
#[derive(Clone)]
pub struct TdxBoundlessConfig {
    /// Boundless Network RPC URL.
    pub rpc_url: Url,
    /// Signer for Boundless Network proving fees.
    pub signer: PrivateKeySigner,
    /// HTTP(S) URL of the TDX attestation verifier ELF.
    pub verifier_program_url: Url,
    /// Expected image ID of the TDX verifier guest program.
    pub image_id: [u32; 8],
    /// Interval between fulfillment status checks.
    pub poll_interval: Duration,
    /// Proof generation timeout.
    pub timeout: Duration,
    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    pub max_recovery_attempts: u32,
    /// Maximum accepted age for recovered TDX quote proofs.
    pub max_recovered_quote_age: Duration,
}

impl std::fmt::Debug for TdxBoundlessConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdxBoundlessConfig")
            .field("rpc_url", &url_origin(&self.rpc_url))
            .field("signer", &self.signer.address())
            .field("verifier_program_url", &url_origin(&self.verifier_program_url))
            .field("image_id", &self.image_id)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("max_recovery_attempts", &self.max_recovery_attempts)
            .field("max_recovered_quote_age", &self.max_recovered_quote_age)
            .finish()
    }
}

/// Platform-specific attestation proving configuration.
#[derive(Clone, Debug)]
pub enum PlatformProvingConfig {
    /// AWS Nitro attestation proving.
    Nitro(ProvingConfig),
    /// Intel TDX attestation proving.
    Tdx(TdxProvingConfig),
}

/// Runtime configuration for one prover fleet.
#[derive(Clone, Debug)]
pub struct PlatformRegistrationConfig {
    /// TEE platform expected from every endpoint in the fleet.
    pub attestation_kind: SignerAttestationKind,
    /// Platform-specific discovery source.
    pub discovery: DiscoveryConfig,
    /// Platform-specific attestation proving backend.
    pub proving: PlatformProvingConfig,
}

/// CRL (Certificate Revocation List) checking configuration.
#[derive(Clone, Debug)]
pub struct CrlConfig {
    /// Whether CRL checking is enabled. When disabled, no CRL fetches or
    /// `revokeCert` transactions are attempted. Defaults to `false`.
    pub enabled: bool,
    /// `NitroEnclaveVerifier` contract address on L1 for `revokeCert` calls.
    /// Required when `enabled` is `true`.
    pub nitro_verifier_address: Option<Address>,
    /// HTTP timeout for CRL fetches from AWS S3 endpoints.
    pub fetch_timeout: Duration,
}

/// Runtime configuration for the prover registrar.
///
/// Constructed by the CLI layer (`bin/prover-registrar`), which handles argument
/// parsing, validation, and signing config resolution before building this type.
pub struct RegistrarConfig {
    // ── L1 ────────────────────────────────────────────────────────────────────
    /// L1 Ethereum RPC endpoint.
    pub l1_rpc_url: Url,
    /// `TEEProverRegistry` contract address on L1.
    pub tee_prover_registry_address: Address,
    /// L1 chain ID (validated against the RPC provider at startup).
    pub l1_chain_id: u64,
    // ── Fleets ────────────────────────────────────────────────────────────────
    /// Platform-specific prover fleets to discover and register.
    pub fleets: Vec<PlatformRegistrationConfig>,
    // ── Signing / Tx Manager ──────────────────────────────────────────────────
    /// Signing configuration (local private key or remote sidecar).
    pub signing: SignerConfig,
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    pub tx_manager: TxManagerConfig,
    // ── Polling / Server ──────────────────────────────────────────────────────
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Timeout for JSON-RPC calls to prover instances.
    pub prover_timeout: Duration,
    /// Maximum number of instances to process concurrently within a single
    /// registration cycle. Each instance may trigger a ~20-minute proof
    /// generation, so this limits concurrent proof work and nonce acquisition.
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
    /// Duration after launch during which unhealthy instances are still
    /// eligible for registration.
    pub unhealthy_registration_window: Duration,
    /// Health server socket address.
    pub health_addr: SocketAddr,
    // ── CRL Checking ──────────────────────────────────────────────────────
    /// CRL (Certificate Revocation List) checking configuration.
    pub crl: CrlConfig,
    /// TDX collateral retrieval and verifier policy configuration.
    pub tdx_attestation: TdxAttestationConfig,
}

impl std::fmt::Debug for RegistrarConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrarConfig")
            .field("l1_rpc_url", &url_origin(&self.l1_rpc_url))
            .field("tee_prover_registry_address", &self.tee_prover_registry_address)
            .field("l1_chain_id", &self.l1_chain_id)
            .field("fleets", &self.fleets)
            .field("signing", &self.signing)
            .field("tx_manager", &self.tx_manager)
            .field("poll_interval", &self.poll_interval)
            .field("prover_timeout", &self.prover_timeout)
            .field("max_concurrency", &self.max_concurrency)
            .field("max_tx_retries", &self.max_tx_retries)
            .field("tx_retry_delay", &self.tx_retry_delay)
            .field("unhealthy_registration_window", &self.unhealthy_registration_window)
            .field("health_addr", &self.health_addr)
            .field("crl", &self.crl)
            .field("tdx_attestation", &self.tdx_attestation)
            .finish()
    }
}

/// Format only the `scheme://host:port` of a URL, dropping the path and query
/// string to avoid leaking embedded API keys (e.g. Infura/Alchemy paths).
pub(crate) fn url_origin(url: &Url) -> String {
    let mut s = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("<unknown>"));
    if let Some(port) = url.port() {
        s.push_str(&format!(":{port}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use base_proof_tee_tdx_collateral::DEFAULT_TDX_TRUSTED_ROOT_CA_HASH;

    use super::*;

    #[test]
    fn intel_pcs_pins_production_root_ca_hash() {
        let config = TdxAttestationConfig::intel_pcs();

        assert_eq!(config.trusted_root_ca_hash, DEFAULT_TDX_TRUSTED_ROOT_CA_HASH);
    }
}
