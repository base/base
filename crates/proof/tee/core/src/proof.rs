use alloy_primitives::{B256, Bytes, U256};

use crate::CryptoError;

/// Length of an ECDSA signature in bytes (r + s + v).
pub const ECDSA_SIGNATURE_LENGTH: usize = 65;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
pub const ECDSA_V_OFFSET: u8 = 27;

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
    use super::*;

    fn test_signature(v: u8) -> Bytes {
        let mut sig = vec![0xAB; 65];
        sig[64] = v;
        Bytes::from(sig)
    }

    #[test]
    fn test_encode_proof_bytes_length() {
        let sig = test_signature(0);
        let proof =
            ProofEncoder::encode_proof_bytes(&sig, B256::repeat_byte(0xCC), U256::from(500))
                .unwrap();
        assert_eq!(proof.len(), 130);
    }

    #[test]
    fn test_encode_proof_bytes_type() {
        let sig = test_signature(0);
        let proof =
            ProofEncoder::encode_proof_bytes(&sig, B256::repeat_byte(0xCC), U256::from(500))
                .unwrap();
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
        // Full 32-byte field should match the U256 big-endian encoding
        assert_eq!(&proof[33..65], &l1_origin_number.to_be_bytes::<32>());
    }

    #[test]
    fn test_encode_proof_bytes_v_zero_adjusted_to_27() {
        let sig = test_signature(0);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO).unwrap();
        assert_eq!(proof[129], 27);
    }

    #[test]
    fn test_encode_proof_bytes_v_one_adjusted_to_28() {
        let sig = test_signature(1);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO).unwrap();
        assert_eq!(proof[129], 28);
    }

    #[test]
    fn test_encode_proof_bytes_v_27_unchanged() {
        let sig = test_signature(27);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO).unwrap();
        assert_eq!(proof[129], 27);
    }

    #[test]
    fn test_encode_proof_bytes_v_28_unchanged() {
        let sig = test_signature(28);
        let proof = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO).unwrap();
        assert_eq!(proof[129], 28);
    }

    #[test]
    fn test_encode_proof_bytes_invalid_v() {
        let sig = test_signature(5);
        let result = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid ECDSA v-value"));
    }

    #[test]
    fn test_encode_proof_bytes_short_signature() {
        let sig = Bytes::from(vec![0u8; 32]);
        let result = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid signature length"));
    }

    #[test]
    fn test_encode_proof_bytes_oversized_signature() {
        let sig = Bytes::from(vec![0u8; 70]);
        let result = ProofEncoder::encode_proof_bytes(&sig, B256::ZERO, U256::ZERO);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid signature length"));
    }
}
