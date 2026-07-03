//! Shared TEE attestation proof types and provider trait.

use std::error::Error;

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;

/// Boxed error type used by platform-neutral attestation providers.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// Convenience result alias for platform-neutral attestation providers.
pub type Result<T> = std::result::Result<T, BoxError>;

/// Supported TEE attestation proof families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeeAttestationKind {
    /// AWS Nitro Enclave attestation proof.
    Nitro,
    /// Intel TDX attestation proof.
    Tdx,
}

/// A generated TEE attestation proof ready for on-chain signer registration.
#[derive(Debug)]
pub struct TeeAttestationProof {
    /// ABI-encoded verifier journal containing the verified attestation data.
    pub output: Bytes,
    /// ZK proof bytes for on-chain verification.
    pub proof_bytes: Bytes,
}

/// Trait for generating TEE attestation proofs for signer registration.
#[async_trait]
pub trait TeeAttestationProofProvider: Send + Sync {
    /// Generates a ZK proof for the given raw attestation bytes and signer.
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> Result<TeeAttestationProof>;

    /// Marks a signer's recovered proof as failed on-chain.
    ///
    /// Implementations that support proof recovery should skip recovery for
    /// this signer on subsequent calls and generate a fresh proof instead.
    /// Implementations without recovery support can use the default no-op.
    fn block_recovery_for_signer(&self, _signer: Address) {}
}
