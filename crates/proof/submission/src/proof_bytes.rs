//! Proof byte helpers for aggregate verifier submissions.

use alloy_primitives::Bytes;
use base_proof_primitives::{PROOF_TYPE_ZK, ProofEncoder};

use crate::ProofSubmissionError;

/// Helpers for encoding compact proof bytes accepted by dispute game entry points.
#[derive(Debug)]
pub struct ProofBytes;

impl ProofBytes {
    /// Encodes a TEE signature for `verifyProposalProof(bytes)`.
    ///
    /// The resulting bytes use the compact dispute-proof format:
    /// `proofType(1) + signature(65)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the signature length or ECDSA v-value is invalid.
    pub fn tee_signature(signature: &[u8]) -> Result<Bytes, ProofSubmissionError> {
        Ok(ProofEncoder::encode_dispute_proof_bytes(signature)?)
    }

    /// Prefixes raw ZK proof bytes with the aggregate verifier ZK proof type byte.
    pub fn zk(proof: impl AsRef<[u8]>) -> Bytes {
        let proof = proof.as_ref();
        let mut raw = Vec::with_capacity(1 + proof.len());
        raw.push(PROOF_TYPE_ZK);
        raw.extend_from_slice(proof);
        Bytes::from(raw)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use base_proof_primitives::{PROOF_TYPE_TEE, PROOF_TYPE_ZK};

    use super::ProofBytes;

    #[test]
    fn tee_signature_encodes_compact_dispute_proof_bytes() {
        let mut signature = vec![0xab; 65];
        signature[64] = 0;

        let proof_bytes = ProofBytes::tee_signature(&signature).unwrap();

        assert_eq!(proof_bytes.len(), 66);
        assert_eq!(proof_bytes[0], PROOF_TYPE_TEE);
        assert_eq!(proof_bytes[65], 27);
    }

    #[test]
    fn tee_signature_rejects_invalid_signature() {
        let err = ProofBytes::tee_signature(&[0xab; 64]).unwrap_err();

        assert!(err.to_string().contains("invalid signature length"));
    }

    #[test]
    fn zk_prefixes_zk_proof_type() {
        let proof_bytes = ProofBytes::zk(Bytes::from_static(&[0xab, 0xcd]));

        assert_eq!(proof_bytes.as_ref(), &[PROOF_TYPE_ZK, 0xab, 0xcd]);
    }
}
