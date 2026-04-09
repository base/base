//! TEE proof encoding for the `AggregateVerifier` contract.

use alloc::vec;

use alloy_primitives::{B256, Bytes, U256};
use thiserror::Error;

use crate::ECDSA_SIGNATURE_LENGTH;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
const ECDSA_V_OFFSET: u8 = 27;

/// Proof type byte for TEE proofs (matches `AggregateVerifier.ProofType.TEE`).
pub const PROOF_TYPE_TEE: u8 = 0;

/// Proof type byte for ZK proofs (matches `AggregateVerifier.ProofType.ZK`).
pub const PROOF_TYPE_ZK: u8 = 1;

/// Errors that can occur during cryptographic operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum CryptoError {
    /// Signature has invalid length.
    #[error("invalid signature length: expected 65 bytes, got {0}")]
    InvalidSignatureLength(usize),

    /// Invalid ECDSA v-value.
    #[error("invalid ECDSA v-value: expected 0, 1, 27, or 28, got {0}")]
    InvalidVValue(u8),
}

/// Proof encoding utilities for TEE proofs.
#[derive(Debug)]
pub struct ProofEncoder;

impl ProofEncoder {
    /// Normalizes an ECDSA v-value from 0/1 to 27/28.
    ///
    /// Values already in the 27/28 range are returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if the v-value is not 0, 1, 27, or 28.
    pub const fn normalize_v(v: u8) -> Result<u8, CryptoError> {
        match v {
            0 | 1 => Ok(v + ECDSA_V_OFFSET),
            27 | 28 => Ok(v),
            _ => Err(CryptoError::InvalidVValue(v)),
        }
    }

    /// Encodes a TEE proof into the 130-byte format expected by
    /// `AggregateVerifier.initializeWithInitData()`.
    ///
    /// Format: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)`
    ///
    /// The v-value in the ECDSA signature is adjusted from 0/1 to 27/28 if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the signature is not exactly 65 bytes or has an invalid v-value.
    pub fn encode_proof_bytes(
        signature: &[u8],
        l1_origin_hash: B256,
        l1_origin_number: u64,
    ) -> Result<Bytes, CryptoError> {
        if signature.len() != ECDSA_SIGNATURE_LENGTH {
            return Err(CryptoError::InvalidSignatureLength(signature.len()));
        }

        let mut proof_data = vec![0u8; 1 + 32 + 32 + ECDSA_SIGNATURE_LENGTH];

        // Byte 0: proof type (TEE = 0)
        proof_data[0] = PROOF_TYPE_TEE;

        // Bytes 1-32: L1 origin hash
        proof_data[1..33].copy_from_slice(l1_origin_hash.as_slice());

        // Bytes 33-64: L1 origin number as 32-byte big-endian uint256
        proof_data[33..65].copy_from_slice(&U256::from(l1_origin_number).to_be_bytes::<32>());

        // Bytes 65-129: ECDSA signature with v-value adjusted from 0/1 to 27/28
        proof_data[65..130].copy_from_slice(&signature[..ECDSA_SIGNATURE_LENGTH]);
        proof_data[129] = Self::normalize_v(proof_data[129])?;

        Ok(Bytes::from(proof_data))
    }

    /// Encodes a TEE proof into the compact 66-byte format expected by
    /// `AggregateVerifier.nullify()`, `challenge()`, and `verifyProposalProof()`.
    ///
    /// Format: `proofType(1) + signature(65)`
    ///
    /// These contract entry-points already have `l1Head` stored in CWIA, so the
    /// proof bytes do not need to carry `l1OriginHash` or `l1OriginNumber`.
    /// The contract slices `proofBytes[1:]` to extract the signature, unlike
    /// `initializeWithInitData` which slices `proof[65:]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the signature is not exactly 65 bytes or has an invalid v-value.
    pub fn encode_dispute_proof_bytes(signature: &[u8]) -> Result<Bytes, CryptoError> {
        if signature.len() != ECDSA_SIGNATURE_LENGTH {
            return Err(CryptoError::InvalidSignatureLength(signature.len()));
        }

        let mut proof_data = vec![0u8; 1 + ECDSA_SIGNATURE_LENGTH];

        // Byte 0: proof type (TEE = 0)
        proof_data[0] = PROOF_TYPE_TEE;

        // Bytes 1-65: ECDSA signature with v-value adjusted from 0/1 to 27/28
        proof_data[1..66].copy_from_slice(&signature[..ECDSA_SIGNATURE_LENGTH]);
        proof_data[65] = Self::normalize_v(proof_data[65])?;

        Ok(Bytes::from(proof_data))
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec, vec::Vec};

    use rstest::rstest;

    use super::*;

    fn test_signature(v: u8) -> Bytes {
        let mut sig = vec![0xAB; 65];
        sig[64] = v;
        Bytes::from(sig)
    }

    #[test]
    fn test_encode_proof_bytes_format() {
        let sig = test_signature(0);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::repeat_byte(0xCC), 500).unwrap();
        assert_eq!(proof.len(), 130);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[test]
    fn test_encode_proof_bytes_l1_origin_hash() {
        let l1_hash = B256::repeat_byte(0xDD);
        let sig = test_signature(0);
        let proof = ProofEncoder::encode_proof_bytes(&sig, l1_hash, 500).unwrap();
        assert_eq!(&proof[1..33], l1_hash.as_slice());
    }

    #[test]
    fn test_encode_proof_bytes_l1_origin_number() {
        let sig = test_signature(0);
        let l1_origin_number = 12345u64;
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, l1_origin_number).unwrap();
        assert_eq!(&proof[33..65], &U256::from(l1_origin_number).to_be_bytes::<32>());
    }

    #[rstest]
    #[case::v_zero_adjusted_to_27(0, 27)]
    #[case::v_one_adjusted_to_28(1, 28)]
    #[case::v_27_unchanged(27, 27)]
    #[case::v_28_unchanged(28, 28)]
    fn test_encode_proof_bytes_v_value(#[case] input_v: u8, #[case] expected_v: u8) {
        let sig = test_signature(input_v);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, 0).unwrap();
        assert_eq!(proof[129], expected_v);
    }

    #[rstest]
    #[case::invalid_v(vec![0xAB; 64].into_iter().chain(core::iter::once(5)).collect::<Vec<_>>(), "invalid ECDSA v-value")]
    #[case::short_signature(vec![0u8; 32], "invalid signature length")]
    #[case::oversized_signature(vec![0u8; 70], "invalid signature length")]
    fn test_encode_proof_bytes_errors(#[case] sig: Vec<u8>, #[case] expected_err: &str) {
        let result = ProofEncoder::encode_proof_bytes(&Bytes::from(sig), B256::ZERO, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(expected_err));
    }

    // --- encode_dispute_proof_bytes tests ---

    #[test]
    fn test_encode_dispute_proof_bytes_format() {
        let sig = test_signature(0);
        let proof = ProofEncoder::encode_dispute_proof_bytes(&sig).unwrap();
        assert_eq!(proof.len(), 66);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[test]
    fn test_encode_dispute_proof_bytes_signature() {
        let mut raw_sig = vec![0xAB; 65];
        raw_sig[64] = 1;
        let proof = ProofEncoder::encode_dispute_proof_bytes(&raw_sig).unwrap();
        // r and s should be preserved
        assert_eq!(&proof[1..65], &raw_sig[..64]);
        // v should be adjusted from 1 to 28
        assert_eq!(proof[65], 28);
    }

    #[rstest]
    #[case::v_zero_adjusted_to_27(0, 27)]
    #[case::v_one_adjusted_to_28(1, 28)]
    #[case::v_27_unchanged(27, 27)]
    #[case::v_28_unchanged(28, 28)]
    fn test_encode_dispute_proof_bytes_v_value(#[case] input_v: u8, #[case] expected_v: u8) {
        let sig = test_signature(input_v);
        let proof = ProofEncoder::encode_dispute_proof_bytes(&sig).unwrap();
        assert_eq!(proof[65], expected_v);
    }

    #[rstest]
    #[case::invalid_v(vec![0xAB; 64].into_iter().chain(core::iter::once(5)).collect::<Vec<_>>(), "invalid ECDSA v-value")]
    #[case::short_signature(vec![0u8; 32], "invalid signature length")]
    #[case::oversized_signature(vec![0u8; 70], "invalid signature length")]
    fn test_encode_dispute_proof_bytes_errors(#[case] sig: Vec<u8>, #[case] expected_err: &str) {
        let result = ProofEncoder::encode_dispute_proof_bytes(&sig);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(expected_err));
    }
}
