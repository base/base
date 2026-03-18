//! [`DirectProver`] — proving backend using `risc0_zkvm::default_prover()`.
//!
//! Routes automatically based on environment variables:
//! - `BONSAI_API_KEY` + `BONSAI_API_URL` → remote Bonsai proving (Groth16)
//! - `RISC0_DEV_MODE=1` → mock/fake prover (testing)
//!
//! Local CPU proving without Bonsai is not supported for on-chain proofs —
//! Groth16 compression requires the Bonsai service or Docker with ceremony
//! files.

use std::sync::Arc;

use alloy_primitives::Bytes;
use base_proof_tee_nitro_verifier::VerifierInput;
use risc0_zkvm::{ExecutorEnv, ProverOpts, compute_image_id, default_prover};

use crate::{AttestationProof, AttestationProofProvider, ProverError, Result};

/// Attestation prover using the RISC Zero default prover.
///
/// The default prover routes to Bonsai remote proving or dev-mode depending
/// on environment variable configuration. Always requests Groth16 receipts
/// for on-chain verifiability.
///
/// Proving is offloaded to a blocking task via [`tokio::task::spawn_blocking`]
/// to avoid stalling the async executor.
#[derive(Debug)]
pub struct DirectProver {
    elf: Arc<[u8]>,
    image_id: [u32; 8],
    trusted_certs_prefix_len: u8,
}

impl DirectProver {
    /// Creates a new [`DirectProver`] from raw guest ELF bytes.
    ///
    /// Computes the image ID from the ELF. The `trusted_certs_prefix_len`
    /// controls how many certificates in the chain are treated as trusted
    /// (typically 1 for root-only).
    pub fn new(elf: Vec<u8>, trusted_certs_prefix_len: u8) -> Result<Self> {
        let digest = compute_image_id(&elf)
            .map_err(|e| ProverError::ImageId(format!("failed to compute image ID: {e}")))?;
        let image_id: [u32; 8] = digest.into();

        Ok(Self { elf: Arc::from(elf), image_id, trusted_certs_prefix_len })
    }

    /// Returns the computed image ID for this guest ELF.
    pub const fn image_id(&self) -> &[u32; 8] {
        &self.image_id
    }
}

#[async_trait::async_trait]
impl AttestationProofProvider for DirectProver {
    async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof> {
        let elf = Arc::clone(&self.elf);
        let trusted_certs_prefix_len = self.trusted_certs_prefix_len;
        let attestation_owned = attestation_bytes.to_vec();

        // Proving is synchronous and potentially long-running (Bonsai HTTP
        // polling or local CPU). Offload to a blocking thread so we don't
        // stall the async executor.
        let (journal_bytes, seal) = tokio::task::spawn_blocking(move || {
            let input = VerifierInput {
                trustedCertsPrefixLen: trusted_certs_prefix_len,
                attestationReport: Bytes::from(attestation_owned),
            };
            let input_bytes = input.encode();

            let env = ExecutorEnv::builder()
                .write_slice(&input_bytes)
                .build()
                .map_err(|e| ProverError::Risc0(format!("failed to build executor env: {e}")))?;

            let prover = default_prover();
            let prove_info = prover
                .prove_with_opts(env, &elf, &ProverOpts::groth16())
                .map_err(|e| ProverError::Risc0(format!("proving failed: {e}")))?;

            let journal = prove_info.receipt.journal.bytes.clone();
            let seal = risc0_ethereum_contracts::encode_seal(&prove_info.receipt)
                .map_err(|e| ProverError::Risc0(format!("failed to encode seal: {e}")))?;

            Ok::<_, ProverError>((journal, seal))
        })
        .await
        .map_err(|e| ProverError::Risc0(format!("proving task panicked: {e}")))??;

        Ok(AttestationProof { output: Bytes::from(journal_bytes), proof_bytes: Bytes::from(seal) })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Default trusted certificate prefix length (root-only).
    const DEFAULT_TRUSTED_PREFIX: u8 = 1;

    // ── DirectProver::new() error paths ─────────────────────────────────

    #[rstest]
    fn new_with_empty_elf_returns_image_id_error() {
        let result = DirectProver::new(vec![], DEFAULT_TRUSTED_PREFIX);

        let err = result.unwrap_err();
        assert!(matches!(err, ProverError::ImageId(_)));
        assert!(
            err.to_string().contains("image ID"),
            "error message should mention image ID: {err}"
        );
    }

    #[rstest]
    fn new_with_garbage_bytes_returns_image_id_error() {
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = DirectProver::new(garbage, DEFAULT_TRUSTED_PREFIX);

        let err = result.unwrap_err();
        assert!(matches!(err, ProverError::ImageId(_)));
    }

    #[rstest]
    #[case::single_zero(vec![0x00])]
    #[case::short_header(vec![0x7F, 0x45, 0x4C, 0x46])] // ELF magic without body
    #[case::random_noise(vec![0xFF; 64])]
    fn new_with_invalid_elf_variants_rejected(#[case] bad_elf: Vec<u8>) {
        let result = DirectProver::new(bad_elf, DEFAULT_TRUSTED_PREFIX);
        assert!(result.is_err(), "invalid ELF should be rejected");
    }

    // ── DirectProver::new() with different trusted prefix lengths ───────

    #[rstest]
    fn new_rejects_invalid_elf_regardless_of_prefix(#[values(0, 1, 2, 5)] trusted_prefix: u8) {
        let result = DirectProver::new(vec![], trusted_prefix);
        assert!(result.is_err());
    }
}
