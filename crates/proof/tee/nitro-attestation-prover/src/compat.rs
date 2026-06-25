//! Compatibility types for existing Nitro registrar callers.

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_tee_attestation::{BoxError, TeeAttestationProof, TeeAttestationProofProvider};
use tokio_util::sync::CancellationToken;

use crate::{ProverError, Result};

/// A generated Nitro attestation proof ready for onchain submission.
pub type AttestationProof = TeeAttestationProof;

/// Legacy Nitro proof provider API used by the registrar.
#[async_trait]
pub trait AttestationProofProvider: Send + Sync {
    /// Generates a ZK proof for the given raw attestation document bytes.
    async fn generate_proof(
        &self,
        attestation_bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<AttestationProof>;

    /// Generates a ZK proof with knowledge of the target signer address.
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
        cancel: &CancellationToken,
    ) -> Result<AttestationProof> {
        let _ = signer_address;
        self.generate_proof(attestation_bytes, cancel).await
    }

    /// Marks a signer's recovered proof as failed onchain.
    fn block_recovery_for_signer(&self, _signer: Address) {}
}

#[async_trait]
impl<T> AttestationProofProvider for T
where
    T: TeeAttestationProofProvider + ?Sized,
{
    async fn generate_proof(
        &self,
        attestation_bytes: &[u8],
        _cancel: &CancellationToken,
    ) -> Result<AttestationProof> {
        TeeAttestationProofProvider::generate_proof_for_signer(
            self,
            attestation_bytes,
            Address::ZERO,
        )
        .await
        .map_err(box_error_to_prover_error)
    }

    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
        _cancel: &CancellationToken,
    ) -> Result<AttestationProof> {
        TeeAttestationProofProvider::generate_proof_for_signer(
            self,
            attestation_bytes,
            signer_address,
        )
        .await
        .map_err(box_error_to_prover_error)
    }

    fn block_recovery_for_signer(&self, signer: Address) {
        TeeAttestationProofProvider::block_recovery_for_signer(self, signer);
    }
}

fn box_error_to_prover_error(error: BoxError) -> ProverError {
    match error.downcast::<ProverError>() {
        Ok(error) => *error,
        Err(error) => ProverError::Risc0(error.to_string()),
    }
}
