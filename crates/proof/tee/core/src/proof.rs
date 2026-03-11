use alloy_primitives::{B256, Bytes, U256};
use base_proof_primitives::ECDSA_SIGNATURE_LENGTH;

use crate::CryptoError;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
pub(crate) const ECDSA_V_OFFSET: u8 = 27;

/// Proof type byte for TEE proofs (matches `AggregateVerifier.ProofType.TEE`).
pub const PROOF_TYPE_TEE: u8 = 0;

/// Proof encoding utilities for TEE proofs.
#[derive(Debug)]
pub struct ProofEncoder;

impl ProofEncoder {
    /// Encodes a TEE proof into the 130-byte format expected by `AggregateVerifier`.
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
        l1_origin_number: U256,
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
        proof_data[33..65].copy_from_slice(&l1_origin_number.to_be_bytes::<32>());

        // Bytes 65-129: ECDSA signature with v-value adjusted from 0/1 to 27/28
        proof_data[65..130].copy_from_slice(&signature[..ECDSA_SIGNATURE_LENGTH]);
        proof_data[129] = match proof_data[129] {
            0 | 1 => proof_data[129] + ECDSA_V_OFFSET,
            27 | 28 => proof_data[129],
            v => return Err(CryptoError::InvalidVValue(v)),
        };

        Ok(Bytes::from(proof_data))
    }
}

#[cfg(test)]
mod tests {
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
        let proof =
            ProofEncoder::encode_proof_bytes(&sig, B256::repeat_byte(0xCC), U256::from(500))
                .unwrap();
        assert_eq!(proof.len(), 130);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[test]
    fn test_encode_proof_bytes_l1_origin_hash() {
        let l1_hash = B256::repeat_byte(0xDD);
        let sig = test_signature(0);
        let proof = ProofEncoder::encode_proof_bytes(&sig, l1_hash, U256::from(500)).unwrap();
        assert_eq!(&proof[1..33], l1_hash.as_slice());
    }

    #[test]
    fn test_encode_proof_bytes_l1_origin_number() {
        let sig = test_signature(0);
        let l1_origin_number = U256::from(12345);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, l1_origin_number).unwrap();
        assert_eq!(&proof[33..65], &l1_origin_number.to_be_bytes::<32>());
    }

    #[rstest]
    #[case::v_zero_adjusted_to_27(0, 27)]
    #[case::v_one_adjusted_to_28(1, 28)]
    #[case::v_27_unchanged(27, 27)]
    #[case::v_28_unchanged(28, 28)]
    fn test_encode_proof_bytes_v_value(#[case] input_v: u8, #[case] expected_v: u8) {
        let sig = test_signature(input_v);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO).unwrap();
        assert_eq!(proof[129], expected_v);
    }

    #[rstest]
    #[case::invalid_v(vec![0xAB; 64].into_iter().chain(std::iter::once(5)).collect::<Vec<_>>(), "invalid ECDSA v-value")]
    #[case::short_signature(vec![0u8; 32], "invalid signature length")]
    #[case::oversized_signature(vec![0u8; 70], "invalid signature length")]
    fn test_encode_proof_bytes_errors(#[case] sig: Vec<u8>, #[case] expected_err: &str) {
        let result = ProofEncoder::encode_proof_bytes(&Bytes::from(sig), B256::ZERO, U256::ZERO);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(expected_err));
    }
}
