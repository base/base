use alloy_primitives::{Address, B256};
use url::Url;

/// A prover instance discovered from the infrastructure layer.
#[derive(Debug, Clone)]
pub struct ProverInstance {
    /// EC2 instance ID (e.g. `i-0abc123def456`).
    pub instance_id: String,
    /// HTTP endpoint URL for the prover (e.g. `http://10.0.1.5:8000/`).
    pub endpoint: Url,
    /// Current health status from discovery or direct readiness probing.
    pub health_status: InstanceHealthStatus,
}

/// Health status of a discovered prover instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceHealthStatus {
    /// ALB health checks are in progress — instance just started.
    Initial,
    /// Instance is reachable and passing readiness checks.
    Healthy,
    /// Instance did not respond to `readyz` or is failing health checks.
    Unhealthy,
    /// ALB is draining connections from this instance.
    Draining,
}

/// Identifies which `CertManager` cache helper a certificate requires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertKind {
    /// Intermediate / non-root CA certificate.
    Ca,
    /// Attestation document leaf (client) certificate.
    Leaf,
}

/// One certificate-cache step in dependency order.
///
/// `cert_hash` is the `CertManager` cache key: full-DER keccak for the pinned root
/// (used only as `parent_cert_hash` of the first CA) and `TBSCertificate` keccak for
/// every non-root certificate. `revocation_id` is `computeCertId` for the cert.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CertPlan {
    /// Cache helper kind.
    pub kind: CertKind,
    /// Human-readable role label (regional CA, leaf, …).
    pub label: String,
    /// DER-encoded certificate bytes.
    pub cert: Vec<u8>,
    /// `CertManager` cache key for this certificate.
    pub cert_hash: B256,
    /// `CertManager` cache key of the parent certificate.
    pub parent_cert_hash: B256,
    /// Issuer/serial revocation identity (`CertManager.computeCertId`).
    pub revocation_id: B256,
}

/// Complete registration plan for one Nitro attestation (no signature hints).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationPlan {
    /// Signer address derived from attestation `public_key`.
    pub signer: Address,
    /// PCR0 measurement bytes.
    pub pcr0: Vec<u8>,
    /// Attestation timestamp (Unix milliseconds).
    pub timestamp: u64,
    /// Optional nonce from the attestation document.
    pub nonce: Option<Vec<u8>>,
    /// Pinned root certificate cache key (`keccak256(full DER)`).
    pub root_cert_hash: B256,
    /// Leaf certificate cache key (`keccak256(TBSCertificate TLV)`).
    pub leaf_cert_hash: B256,
    /// COSE `Sig_structure` bytes (attestation TBS).
    pub attestation_tbs: Vec<u8>,
    /// 96-byte P-384 signature (`r || s`).
    pub signature: Vec<u8>,
    /// Non-root CAs (parent-first) followed by the leaf certificate.
    pub certs: Vec<CertPlan>,
}

/// Packed P-384 inverse-hint streams for one registration plan.
///
/// Each stream is `inverse_0 ‖ … ‖ inverse_{k-1}` with 48-byte big-endian limbs
/// in the exact order consumed by onchain `P384Verifier`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationHints {
    /// Hint stream for each entry in [`RegistrationPlan::certs`] (same order).
    pub cert_signature_hints: Vec<Vec<u8>>,
    /// Hint stream for the attestation COSE signature.
    pub attestation_hints: Vec<u8>,
}

/// Registration plan plus `CertManager` / attestation inverse-hint streams.
///
/// Produced by [`crate::AttestationPlanner::prepare_hinted_registration_plan`].
/// Not consumed by the running Boundless registrar path until orchestration lands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HintedRegistrationPlan {
    /// Certificate / signer plan (no hints).
    pub plan: RegistrationPlan,
    /// Packed inverse hints for CA/leaf cert signatures and the attestation sig.
    pub hints: RegistrationHints,
}
