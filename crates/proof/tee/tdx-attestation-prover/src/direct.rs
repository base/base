//! Native direct TDX attestation proof generation for development and tests.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolValue;
use async_trait::async_trait;
use base_proof_tee_attestation::{
    TeeAttestationKind, TeeAttestationProof, TeeAttestationProofProvider,
};
use base_proof_tee_tdx_verifier::{TdxVerifier, TdxVerifierInput};

use crate::{Result, TdxAttestationProverInput};

/// Native direct prover for local development.
///
/// This path runs the TDX verifier in-process and returns the ABI-encoded
/// journal with deterministic development proof bytes. It is intended for
/// local/mock verifier configurations and does not require TDX hardware.
#[derive(Debug, Default)]
pub struct DirectProver;

impl DirectProver {
    /// Generates a TDX attestation proof from an explicit verifier input.
    pub fn generate_proof(input: &TdxVerifierInput) -> Result<TeeAttestationProof> {
        let journal = TdxVerifier::verify(input)?;
        Ok(TeeAttestationProof {
            kind: TeeAttestationKind::Tdx,
            output: Bytes::from(SolValue::abi_encode(&journal)),
            proof_bytes: Bytes::from_static(b"base-tdx-direct-dev-proof-v1"),
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
        Ok(Self::generate_proof(input.verifier_input())?)
    }
}
