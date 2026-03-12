use std::time::Duration;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use url::Url;

/// HTTP signer sidecar configuration (production).
#[derive(Debug, Clone)]
pub struct RemoteSignerConfig {
    /// Signer sidecar JSON-RPC endpoint URL.
    pub endpoint: Url,
    /// Manager address for signing registration transactions.
    pub address: Address,
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

/// Runtime configuration for the prover registrar.
///
/// Constructed by the CLI layer (`bin/prover-registrar`), which handles argument
/// parsing, validation, and signing config resolution before building this type.
#[derive(Clone)]
pub struct RegistrarConfig {
    // ── L1 ────────────────────────────────────────────────────────────────────
    /// L1 Ethereum RPC endpoint.
    pub l1_rpc_url: Url,
    /// `SystemConfigGlobal` contract address on L1.
    pub system_config_global_address: Address,
    // ── AWS ───────────────────────────────────────────────────────────────────
    /// AWS ALB target group ARN for prover instance discovery.
    pub target_group_arn: String,
    /// AWS region.
    pub aws_region: String,
    /// JSON-RPC port to poll on each prover instance.
    pub prover_port: u16,
    // ── Signing ───────────────────────────────────────────────────────────────
    /// Resolved signing configuration.
    pub signing: SigningConfig,
    // ── Boundless ─────────────────────────────────────────────────────────────
    /// Boundless Network RPC URL.
    pub boundless_rpc_url: Url,
    /// Hex-encoded private key for Boundless Network proving fees.
    pub boundless_private_key: String,
    /// IPFS URL of the Nitro attestation verifier ELF uploaded via `nitro-attest-cli`.
    pub boundless_verifier_program_url: Url,
    /// Minimum price in wei per cycle for Boundless proof requests.
    pub boundless_min_price: u64,
    /// Maximum price in wei per cycle for Boundless proof requests.
    pub boundless_max_price: u64,
    /// Proof generation timeout in seconds.
    pub boundless_timeout_secs: u64,
    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    pub nitro_verifier_address: Option<Address>,
    // ── Polling / Server ──────────────────────────────────────────────────────
    /// Interval between discovery and registration poll cycles, in seconds.
    pub poll_interval_secs: u64,
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
            .field("system_config_global_address", &self.system_config_global_address)
            .field("target_group_arn", &self.target_group_arn)
            .field("aws_region", &self.aws_region)
            .field("prover_port", &self.prover_port)
            .field("signing", &self.signing)
            .field("boundless_rpc_url", &url_origin(&self.boundless_rpc_url))
            .field("boundless_private_key", &"<redacted>")
            .field("boundless_verifier_program_url", &self.boundless_verifier_program_url)
            .field("boundless_min_price", &self.boundless_min_price)
            .field("boundless_max_price", &self.boundless_max_price)
            .field("boundless_timeout_secs", &self.boundless_timeout_secs)
            .field("nitro_verifier_address", &self.nitro_verifier_address)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("health_port", &self.health_port)
            .finish()
    }
}

impl RegistrarConfig {
    /// Return the poll interval as a [`Duration`].
    pub const fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    /// Return the Boundless proof generation timeout as a [`Duration`].
    pub const fn boundless_timeout(&self) -> Duration {
        Duration::from_secs(self.boundless_timeout_secs)
    }
}
