//! Native direct TDX attestation proof generation for development and tests.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolValue;
use async_trait::async_trait;
use base_proof_tee_attestation::{
    TeeAttestationKind, TeeAttestationProof, TeeAttestationProofProvider,
};
use base_proof_tee_tdx_verifier::{TdxVerifier, TdxVerifierInput};

use crate::{Result, TdxAttestationProverInput};

/// Default proof bytes used by the native direct prover.
pub const DIRECT_DEV_PROOF_BYTES: &[u8] = b"base-tdx-direct-dev-proof-v1";

/// Native direct prover for local development.
///
/// This path runs the TDX verifier in-process and returns the ABI-encoded
/// journal with deterministic development proof bytes. It is intended for
/// local/mock verifier configurations and does not require TDX hardware.
#[derive(Debug, Default)]
pub struct DirectProver;

impl DirectProver {
    /// Creates a direct prover using the native TDX journal verifier.
    pub fn new() -> Self {
        Self
    }

    /// Generates a TDX attestation proof from an explicit verifier input.
    pub fn generate_proof(&self, input: &TdxVerifierInput) -> Result<TeeAttestationProof> {
        let journal = TdxVerifier::verify(input)?;
        Ok(TeeAttestationProof {
            kind: TeeAttestationKind::Tdx,
            output: Bytes::from(SolValue::abi_encode(&journal)),
            proof_bytes: Bytes::from_static(DIRECT_DEV_PROOF_BYTES),
        })
    }
}

#[async_trait]
impl TeeAttestationProofProvider for DirectProver {
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> base_proof_tee_attestation::Result<TeeAttestationProof> {
        let input =
            TdxAttestationProverInput::decode_for_signer(attestation_bytes, signer_address)?;
        Ok(self.generate_proof(input.verifier_input())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::verifier_input;

    #[tokio::test]
    async fn provider_rejects_mismatched_signer() {
        let input = TdxAttestationProverInput::new(verifier_input());
        let prover = DirectProver::new();

        let error = prover
            .generate_proof_for_signer(&input.encode(), Address::repeat_byte(0x99))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("signer mismatch"));
    }
}
