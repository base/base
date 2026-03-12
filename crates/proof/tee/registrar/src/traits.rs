use async_trait::async_trait;

use crate::{AttestationProof, ProverInstance, Result};

/// Discovers active prover instances from the infrastructure layer.
///
/// The primary implementation is `AwsTargetGroupDiscovery`, which queries
/// an ALB target group via the AWS SDK. Other implementations (e.g., a static
/// list for local testing) can be substituted.
#[async_trait]
pub trait InstanceDiscovery: Send + Sync {
    /// Return the current set of prover instances with their health status.
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>>;
}

/// Generates ZK proofs for Nitro TEE attestation documents.
///
/// The primary implementation is `BoundlessNitroProofProvider`, which submits
/// raw Nitro attestation bytes to the Boundless Network (RISC Zero / Automata
/// SDK) and returns an on-chain-ready [`AttestationProof`].
#[async_trait]
pub trait AttestationProofProvider: Send + Sync {
    /// Generate a ZK proof for the given raw Nitro attestation document.
    ///
    /// `attestation_bytes` is the `COSE_Sign1`-encoded document returned by
    /// `enclave_signerAttestation`. The returned [`AttestationProof`] contains
    /// the ABI-encoded journal and proof bytes ready for `registerSigner`.
    async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof>;
}
