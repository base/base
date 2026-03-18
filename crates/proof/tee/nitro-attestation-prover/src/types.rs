//! Core types and trait for attestation proof generation.

use alloy_primitives::Bytes;
use async_trait::async_trait;

use crate::Result;

/// A generated attestation proof ready for on-chain submission.
#[derive(Debug, Clone)]
pub struct AttestationProof {
    /// ABI-encoded [`VerifierJournal`](base_proof_tee_nitro_verifier::VerifierJournal)
    /// containing the verified attestation data.
    pub output: Bytes,
    /// Groth16 seal bytes for on-chain verification.
    pub proof_bytes: Bytes,
}

/// Trait for generating ZK proofs over Nitro attestation documents.
///
/// Implementors wrap the attestation verification logic inside a ZK proving
/// backend and return a proof suitable for on-chain submission.
#[async_trait]
pub trait AttestationProofProvider: Send + Sync {
    /// Generates a ZK proof for the given raw attestation document bytes.
    async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof>;
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn proof_fields_accessible() {
        let journal = Bytes::from_static(b"journal-data");
        let seal = Bytes::from_static(b"seal-data");

        let proof = AttestationProof { output: journal.clone(), proof_bytes: seal.clone() };

        assert_eq!(proof.output, journal);
        assert_eq!(proof.proof_bytes, seal);
    }

    #[rstest]
    fn proof_clone() {
        let proof = AttestationProof {
            output: Bytes::from_static(b"j"),
            proof_bytes: Bytes::from_static(b"s"),
        };
        let cloned = proof.clone();

        assert_eq!(proof.output, cloned.output);
        assert_eq!(proof.proof_bytes, cloned.proof_bytes);
    }

    #[rstest]
    fn proof_debug_format() {
        let proof = AttestationProof { output: Bytes::new(), proof_bytes: Bytes::new() };
        // Ensure Debug is implemented and doesn't panic.
        let debug = format!("{proof:?}");
        assert!(debug.contains("AttestationProof"));
    }
}
