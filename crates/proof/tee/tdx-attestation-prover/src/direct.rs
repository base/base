//! Native direct TDX attestation proof generation for development and tests.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolValue;
use base_proof_tee_attestation::{TeeAttestationProof, TeeAttestationProofProvider};
use base_proof_tee_tdx_verifier::{TdxVerifier, TdxVerifierInput};

/// Native direct prover for local development.
///
/// This path runs the TDX verifier in-process and returns the ABI-encoded
/// journal with deterministic development proof bytes. It is intended for
/// local/mock verifier configurations and does not require TDX hardware.
#[derive(Debug)]
pub struct DirectProver;

#[async_trait::async_trait]
impl TeeAttestationProofProvider for DirectProver {
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> base_proof_tee_attestation::Result<TeeAttestationProof> {
        let input = TdxVerifierInput::decode_for_signer(attestation_bytes, signer_address)?;
        let journal = TdxVerifier::verify(&input)?;
        Ok(TeeAttestationProof {
            output: Bytes::from(SolValue::abi_encode(&journal)),
            proof_bytes: Bytes::from_static(b"base-tdx-direct-dev-proof-v1"),
        })
    }
}
