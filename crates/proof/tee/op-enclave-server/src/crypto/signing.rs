//! Proposal signing and verification.
//!
//! This module provides functions for signing proposal data and verifying
//! proposal signatures, matching the Go implementation in `server.go`.

use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use k256::ecdsa::signature::hazmat::PrehashVerifier;
use k256::ecdsa::{Signature, VerifyingKey};

use crate::error::ProposalError;

/// Expected length of an ECDSA signature (r: 32 bytes, s: 32 bytes, v: 1 byte).
pub const SIGNATURE_LENGTH: usize = 65;

/// Length of the signing data (5 x 32 bytes = 160 bytes).
pub const SIGNING_DATA_LENGTH: usize = 160;

/// Build the signing data for a proposal.
///
/// The signing data format matches Go's implementation in `server.go:282-285`:
/// ```text
/// configHash || l1OriginHash || l2BlockNumber || prevOutputRoot || outputRoot
/// ```
///
/// Where `l2BlockNumber` is left-padded to 32 bytes (matching `common.BytesToHash`).
///
/// # Arguments
///
/// * `config_hash` - The configuration hash
/// * `l1_origin_hash` - The L1 origin block hash
/// * `l2_block_number` - The L2 block number
/// * `prev_output_root` - The previous output root
/// * `output_root` - The current output root
///
/// # Returns
///
/// A 160-byte array containing the concatenated data.
#[must_use]
pub fn build_signing_data(
    config_hash: B256,
    l1_origin_hash: B256,
    l2_block_number: U256,
    prev_output_root: B256,
    output_root: B256,
) -> [u8; SIGNING_DATA_LENGTH] {
    let mut data = [0u8; SIGNING_DATA_LENGTH];
    data[0..32].copy_from_slice(config_hash.as_slice());
    data[32..64].copy_from_slice(l1_origin_hash.as_slice());
    // L2 block number as big-endian 32 bytes (left-padded, matches common.BytesToHash)
    data[64..96].copy_from_slice(&l2_block_number.to_be_bytes::<32>());
    data[96..128].copy_from_slice(prev_output_root.as_slice());
    data[128..160].copy_from_slice(output_root.as_slice());
    data
}

/// Sign proposal data synchronously.
///
/// Signs the keccak256 hash of the data with the provided signer,
/// returning a 65-byte recoverable signature (r || s || v).
///
/// # Arguments
///
/// * `signer` - The private key signer
/// * `data` - The data to sign (will be hashed with keccak256)
///
/// # Returns
///
/// A 65-byte signature on success.
///
/// # Errors
///
/// Returns an error if signing fails.
pub fn sign_proposal_data_sync(
    signer: &PrivateKeySigner,
    data: &[u8],
) -> Result<Bytes, ProposalError> {
    let hash = keccak256(data);
    let signature = signer
        .sign_hash_sync(&hash)
        .map_err(|e| ProposalError::SigningFailed(e.to_string()))?;

    // Convert to 65-byte format: r (32) || s (32) || v (1)
    // Use as_rsy() which returns v as 0 or 1 (parity bit), matching Go's crypto.Sign format.
    let sig_bytes = signature.as_rsy();

    Ok(Bytes::from(sig_bytes.to_vec()))
}

/// Verify a proposal signature.
///
/// Verifies that the signature was created by the owner of the given public key.
/// This matches Go's `crypto.VerifySignature` which uses only the first 64 bytes
/// of the signature (r, s) and ignores the recovery byte (v).
///
/// # Arguments
///
/// * `public_key_bytes` - The 65-byte uncompressed public key (0x04 || x || y)
/// * `data` - The original signed data (will be hashed with keccak256)
/// * `signature` - The 65-byte signature (r || s || v)
///
/// # Returns
///
/// `true` if the signature is valid, `false` otherwise.
///
/// # Errors
///
/// Returns an error if the signature or public key cannot be parsed.
pub fn verify_proposal_signature(
    public_key_bytes: &[u8],
    data: &[u8],
    signature: &[u8],
) -> Result<bool, ProposalError> {
    if signature.len() != SIGNATURE_LENGTH {
        return Err(ProposalError::InvalidSignatureLength(signature.len()));
    }

    // Parse the public key (expecting 65-byte uncompressed format: 0x04 || x || y)
    let verifying_key = VerifyingKey::from_sec1_bytes(public_key_bytes)
        .map_err(|e| ProposalError::RecoveryFailed(e.to_string()))?;

    // Parse the signature (first 64 bytes only, matching Go's crypto.VerifySignature)
    let sig = Signature::from_slice(&signature[..64])
        .map_err(|e| ProposalError::SigningFailed(format!("failed to parse signature: {e}")))?;

    // Hash the data and verify
    let hash = keccak256(data);
    Ok(verifying_key.verify_prehash(hash.as_slice(), &sig).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;
    use rand::rngs::OsRng;

    use crate::crypto::generate_signer;

    #[test]
    fn test_build_signing_data_length() {
        let data = build_signing_data(B256::ZERO, B256::ZERO, U256::ZERO, B256::ZERO, B256::ZERO);
        assert_eq!(data.len(), SIGNING_DATA_LENGTH);
    }

    #[test]
    fn test_build_signing_data_components() {
        let config_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let l1_origin_hash =
            b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let l2_block_number = U256::from(12345);
        let prev_output_root =
            b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let output_root = b256!("4444444444444444444444444444444444444444444444444444444444444444");

        let data = build_signing_data(
            config_hash,
            l1_origin_hash,
            l2_block_number,
            prev_output_root,
            output_root,
        );

        // Verify each component
        assert_eq!(&data[0..32], config_hash.as_slice());
        assert_eq!(&data[32..64], l1_origin_hash.as_slice());
        assert_eq!(&data[64..96], &l2_block_number.to_be_bytes::<32>());
        assert_eq!(&data[96..128], prev_output_root.as_slice());
        assert_eq!(&data[128..160], output_root.as_slice());
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let mut rng = OsRng;
        let signer = generate_signer(&mut rng).expect("failed to generate signer");

        let data = build_signing_data(
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            U256::from(12345),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        );

        let signature = sign_proposal_data_sync(&signer, &data).expect("signing failed");
        assert_eq!(signature.len(), SIGNATURE_LENGTH);

        let public_key = crate::crypto::public_key_bytes(&signer);
        let valid =
            verify_proposal_signature(&public_key, &data, &signature).expect("verification failed");
        assert!(valid);
    }

    #[test]
    fn test_verify_wrong_key_fails() {
        let mut rng = OsRng;
        let signer1 = generate_signer(&mut rng).expect("failed to generate signer");
        let signer2 = generate_signer(&mut rng).expect("failed to generate signer");

        let data = build_signing_data(
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            U256::from(12345),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        );

        // Sign with signer1
        let signature = sign_proposal_data_sync(&signer1, &data).expect("signing failed");

        // Verify with signer2's public key (should fail)
        let wrong_public_key = crate::crypto::public_key_bytes(&signer2);
        let valid = verify_proposal_signature(&wrong_public_key, &data, &signature)
            .expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_verify_wrong_data_fails() {
        let mut rng = OsRng;
        let signer = generate_signer(&mut rng).expect("failed to generate signer");

        let data1 = build_signing_data(
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            U256::from(12345),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        );

        let data2 = build_signing_data(
            b256!("5555555555555555555555555555555555555555555555555555555555555555"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            U256::from(12345),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        );

        // Sign data1
        let signature = sign_proposal_data_sync(&signer, &data1).expect("signing failed");

        // Verify against data2 (should fail)
        let public_key = crate::crypto::public_key_bytes(&signer);
        let valid = verify_proposal_signature(&public_key, &data2, &signature)
            .expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_invalid_signature_length() {
        let public_key = vec![0x04; 65]; // Dummy public key
        let data = [0u8; 160];
        let short_sig = vec![0u8; 64]; // Too short

        let result = verify_proposal_signature(&public_key, &data, &short_sig);
        assert!(matches!(
            result,
            Err(ProposalError::InvalidSignatureLength(64))
        ));
    }
}
