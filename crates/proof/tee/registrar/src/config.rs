use std::{path::PathBuf, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use url::Url;

/// HTTP signer sidecar configuration (production).
#[derive(Clone)]
pub struct RemoteSignerConfig {
    /// Signer sidecar JSON-RPC endpoint URL.
    pub endpoint: Url,
    /// Manager address for signing registration transactions.
    pub address: Address,
}

impl std::fmt::Debug for RemoteSignerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSignerConfig")
            .field("endpoint", &url_origin(&self.endpoint))
            .field("address", &self.address)
            .finish()
    }
}

/// Resolved signing configuration for L1 transaction submission.
#[derive(Clone)]
pub enum SigningConfig {
    /// HTTP signer sidecar (production).
    Remote(RemoteSignerConfig),
    /// Direct in-process private key. **Development / testing only.**
    Local(PrivateKeySigner),
}

impl std::fmt::Debug for SigningConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote(config) => f
                .debug_struct("Remote")
                .field("endpoint", &url_origin(&config.endpoint))
                .field("address", &config.address)
                .finish(),
            Self::Local(signer) => {
                f.debug_struct("Local").field("address", &signer.address()).finish()
            }
        }
    }
}

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

/// Boundless Network configuration for ZK proof generation.
#[derive(Clone)]
pub struct BoundlessConfig {
    /// Boundless Network RPC URL.
    pub rpc_url: Url,
    /// Signer for Boundless Network proving fees.
    pub signer: PrivateKeySigner,
    /// IPFS URL of the Nitro attestation verifier ELF uploaded via `nitro-attest-cli`.
    pub verifier_program_url: Url,
    /// Expected image ID of the guest program (hex-encoded `[u32; 8]`).
    pub image_id: [u32; 8],
    /// Minimum price in wei per cycle for Boundless proof requests.
    pub min_price: u64,
    /// Maximum price in wei per cycle for Boundless proof requests.
    pub max_price: u64,
    /// Interval between fulfillment status checks.
    pub poll_interval: Duration,
    /// Proof generation timeout.
    pub timeout: Duration,
    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    pub nitro_verifier_address: Option<Address>,
}

impl std::fmt::Debug for BoundlessConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundlessConfig")
            .field("rpc_url", &url_origin(&self.rpc_url))
            .field("signer", &self.signer.address())
            .field("verifier_program_url", &self.verifier_program_url)
            .field("image_id", &self.image_id)
            .field("min_price", &self.min_price)
            .field("max_price", &self.max_price)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("nitro_verifier_address", &self.nitro_verifier_address)
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

/// Runtime configuration for the prover registrar.
///
/// Constructed by the CLI layer (`bin/prover-registrar`), which handles argument
/// parsing, validation, and signing config resolution before building this type.
#[derive(Clone)]
pub struct RegistrarConfig {
    // ── L1 ────────────────────────────────────────────────────────────────────
    /// L1 Ethereum RPC endpoint.
    pub l1_rpc_url: Url,
    /// `TEEProverRegistry` contract address on L1.
    pub tee_prover_registry_address: Address,
    /// L1 chain ID (validated against the RPC provider at startup).
    pub l1_chain_id: u64,
    // ── Discovery ─────────────────────────────────────────────────────────────
    /// AWS ALB target group discovery configuration.
    pub discovery: AwsDiscoveryConfig,
    // ── Signing ───────────────────────────────────────────────────────────────
    /// Resolved signing configuration.
    pub signing: SigningConfig,
    // ── Proving ───────────────────────────────────────────────────────────────
    /// ZK proving backend configuration.
    pub proving: ProvingConfig,
    // ── Polling / Server ──────────────────────────────────────────────────────
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Timeout for JSON-RPC calls to prover instances.
    pub prover_timeout: Duration,
    /// Port for the health check and Prometheus metrics HTTP server.
    pub health_port: u16,
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

impl std::fmt::Debug for RegistrarConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrarConfig")
            .field("l1_rpc_url", &url_origin(&self.l1_rpc_url))
            .field("tee_prover_registry_address", &self.tee_prover_registry_address)
            .field("l1_chain_id", &self.l1_chain_id)
            .field("discovery", &self.discovery)
            .field("signing", &self.signing)
            .field("proving", &self.proving)
            .field("poll_interval", &self.poll_interval)
            .field("prover_timeout", &self.prover_timeout)
            .field("health_port", &self.health_port)
            .finish()
    }
}
