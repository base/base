//! TEE proof encoding for the `AggregateVerifier` contract.

use alloc::vec::Vec;

use alloy_primitives::{B256, Bytes, U256};
use thiserror::Error;

use crate::ECDSA_SIGNATURE_LENGTH;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
const ECDSA_V_OFFSET: u8 = 27;

/// Length of the proof type prefix byte.
const PROOF_TYPE_LEN: usize = 1;

/// Length of the L1 origin hash field.
const L1_ORIGIN_HASH_LEN: usize = 32;

/// Length of the L1 origin number field (uint256).
const L1_ORIGIN_NUMBER_LEN: usize = 32;

/// Combined length of the L1 origin hash and number fields.
const L1_ORIGIN_HEADER_LEN: usize = L1_ORIGIN_HASH_LEN + L1_ORIGIN_NUMBER_LEN;

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

    /// Returns a copy of a 65-byte ECDSA signature with the v-value normalized.
    ///
    /// # Errors
    ///
    /// Returns an error if the signature is not exactly 65 bytes or has an invalid v-value.
    pub fn normalize_signature(
        signature: &[u8],
    ) -> Result<[u8; ECDSA_SIGNATURE_LENGTH], CryptoError> {
        let mut normalized: [u8; ECDSA_SIGNATURE_LENGTH] = signature
            .try_into()
            .map_err(|_| CryptoError::InvalidSignatureLength(signature.len()))?;
        normalized[ECDSA_SIGNATURE_LENGTH - 1] =
            Self::normalize_v(normalized[ECDSA_SIGNATURE_LENGTH - 1])?;
        Ok(normalized)
    }

    /// Encodes a TEE proof with optional L1 origin header and one or more signatures.
    ///
    /// Format: `PROOF_TYPE_TEE(1) [+ l1OriginHash(32) + l1OriginNumber(32)] + signatures(65*N)`.
    fn encode(l1_origin: Option<(B256, u64)>, signatures: &[&[u8]]) -> Result<Bytes, CryptoError> {
        let header_len = if l1_origin.is_some() { L1_ORIGIN_HEADER_LEN } else { 0 };
        let total_len = PROOF_TYPE_LEN + header_len + signatures.len() * ECDSA_SIGNATURE_LENGTH;

        let mut buf = Vec::with_capacity(total_len);
        buf.push(PROOF_TYPE_TEE);

        if let Some((hash, number)) = l1_origin {
            buf.extend_from_slice(hash.as_slice());
            buf.extend_from_slice(&U256::from(number).to_be_bytes::<L1_ORIGIN_NUMBER_LEN>());
        }

        for signature in signatures {
            buf.extend_from_slice(&Self::normalize_signature(signature)?);
        }

        Ok(Bytes::from(buf))
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
        Self::encode(Some((l1_origin_hash, l1_origin_number)), &[signature])
    }

    /// Encodes a dual-platform TEE proof for `AggregateVerifier.initializeWithInitData()`.
    ///
    /// Format: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32)
    /// + nitroSignature(65) + tdxSignature(65)`.
    ///
    /// The v-value in each ECDSA signature is adjusted from 0/1 to 27/28 if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if either signature is not exactly 65 bytes or has an invalid v-value.
    pub fn encode_dual_tee_proof_bytes(
        nitro_signature: &[u8],
        tdx_signature: &[u8],
        l1_origin_hash: B256,
        l1_origin_number: u64,
    ) -> Result<Bytes, CryptoError> {
        Self::encode(Some((l1_origin_hash, l1_origin_number)), &[nitro_signature, tdx_signature])
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
        Self::encode(None, &[signature])
    }

    /// Encodes a compact dual-platform TEE proof for dispute-game entry points.
    ///
    /// Format: `proofType(1) + nitroSignature(65) + tdxSignature(65)`.
    ///
    /// # Errors
    ///
    /// Returns an error if either signature is not exactly 65 bytes or has an invalid v-value.
    pub fn encode_dual_tee_dispute_proof_bytes(
        nitro_signature: &[u8],
        tdx_signature: &[u8],
    ) -> Result<Bytes, CryptoError> {
        Self::encode(None, &[nitro_signature, tdx_signature])
    }

    /// Encodes raw ZK proof bytes into the compact format expected by dispute game entry points.
    ///
    /// Format: `proofType(1) + rawZkProof`.
    pub fn encode_zk_dispute_proof_bytes(proof: impl AsRef<[u8]>) -> Bytes {
        let proof = proof.as_ref();
        let mut proof_data = Vec::with_capacity(PROOF_TYPE_LEN + proof.len());
        proof_data.push(PROOF_TYPE_ZK);
        proof_data.extend_from_slice(proof);
        Bytes::from(proof_data)
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
    #[case::invalid_v(test_signature(5).to_vec(), "invalid ECDSA v-value")]
    #[case::short_signature(vec![0u8; 32], "invalid signature length")]
    #[case::oversized_signature(vec![0u8; 70], "invalid signature length")]
    fn test_encode_proof_bytes_errors(#[case] sig: Vec<u8>, #[case] expected_err: &str) {
        let result = ProofEncoder::encode_proof_bytes(&Bytes::from(sig), B256::ZERO, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(expected_err));
    }

    #[test]
    fn test_encode_dual_tee_proof_bytes_format() {
        let mut nitro_sig = vec![0xAA; 65];
        nitro_sig[64] = 0;
        let mut tdx_sig = vec![0xBB; 65];
        tdx_sig[64] = 1;
        let proof = ProofEncoder::encode_dual_tee_proof_bytes(
            &nitro_sig,
            &tdx_sig,
            B256::repeat_byte(0xCC),
            500,
        )
        .unwrap();

        assert_eq!(proof.len(), 195);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
        assert_eq!(&proof[1..33], B256::repeat_byte(0xCC).as_slice());
        assert_eq!(&proof[33..65], &U256::from(500u64).to_be_bytes::<32>());
        nitro_sig[64] = 27;
        tdx_sig[64] = 28;
        assert_eq!(&proof[65..130], &nitro_sig);
        assert_eq!(&proof[130..195], &tdx_sig);
        assert_eq!(proof[129], 27);
        assert_eq!(proof[194], 28);
    }

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
        assert_eq!(&proof[1..65], &raw_sig[..64]);
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
    #[case::invalid_v(test_signature(5).to_vec(), "invalid ECDSA v-value")]
    #[case::short_signature(vec![0u8; 32], "invalid signature length")]
    #[case::oversized_signature(vec![0u8; 70], "invalid signature length")]
    fn test_encode_dispute_proof_bytes_errors(#[case] sig: Vec<u8>, #[case] expected_err: &str) {
        let result = ProofEncoder::encode_dispute_proof_bytes(&sig);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(expected_err));
    }

    #[test]
    fn test_encode_dual_tee_dispute_proof_bytes_format() {
        let nitro_sig = test_signature(0);
        let tdx_sig = test_signature(1);
        let proof =
            ProofEncoder::encode_dual_tee_dispute_proof_bytes(&nitro_sig, &tdx_sig).unwrap();

        assert_eq!(proof.len(), 131);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
        assert_eq!(proof[65], 27);
        assert_eq!(proof[130], 28);
    }

    #[test]
    fn test_encode_zk_dispute_proof_bytes_prefixes_zk_type() {
        let proof = ProofEncoder::encode_zk_dispute_proof_bytes(Bytes::from_static(&[0xab, 0xcd]));

        assert_eq!(proof.as_ref(), &[PROOF_TYPE_ZK, 0xab, 0xcd]);
    }
}
