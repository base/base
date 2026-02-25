//! Proposal signing and verification.
//!
//! This module provides functions for signing proposal data and verifying
//! proposal signatures, matching the Go implementation in `server.go`.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use k256::ecdsa::{Signature, VerifyingKey, signature::hazmat::PrehashVerifier};

use crate::error::ProposalError;

/// Expected length of an ECDSA signature (r: 32 bytes, s: 32 bytes, v: 1 byte).
pub const SIGNATURE_LENGTH: usize = 65;

/// Base length of the signing data without intermediate roots:
/// address(20) + 8 x bytes32(32) = 276 bytes.
pub const SIGNING_DATA_BASE_LENGTH: usize = 276;

/// Build the signing data for a proposal.
///
/// The format matches the `AggregateVerifier` contract's journal:
/// ```text
/// prover(20) || l1OriginHash(32) || l1OriginNumber(32) || prevOutputRoot(32)
///   || startingL2Block(32) || outputRoot(32) || endingL2Block(32)
///   || intermediateRoots(32*N) || configHash(32) || imageHash(32)
/// ```
///
/// For individual block proofs, `intermediate_roots` is empty.
/// For aggregated proofs, it contains N roots where N = `block_interval` / `intermediate_block_interval`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_signing_data(
    proposer: Address,
    l1_origin_hash: B256,
    l1_origin_number: U256,
    prev_output_root: B256,
    starting_l2_block: U256,
    output_root: B256,
    ending_l2_block: U256,
    intermediate_roots: &[B256],
    config_hash: B256,
    tee_image_hash: B256,
) -> Vec<u8> {
    let len = SIGNING_DATA_BASE_LENGTH + 32 * intermediate_roots.len();
    let mut data = vec![0u8; len];
    let mut offset = 0;

    data[offset..offset + 20].copy_from_slice(proposer.as_slice());
    offset += 20;

    data[offset..offset + 32].copy_from_slice(l1_origin_hash.as_slice());
    offset += 32;

    data[offset..offset + 32].copy_from_slice(&l1_origin_number.to_be_bytes::<32>());
    offset += 32;

    data[offset..offset + 32].copy_from_slice(prev_output_root.as_slice());
    offset += 32;

    data[offset..offset + 32].copy_from_slice(&starting_l2_block.to_be_bytes::<32>());
    offset += 32;

    data[offset..offset + 32].copy_from_slice(output_root.as_slice());
    offset += 32;

    data[offset..offset + 32].copy_from_slice(&ending_l2_block.to_be_bytes::<32>());
    offset += 32;

    for root in intermediate_roots {
        data[offset..offset + 32].copy_from_slice(root.as_slice());
        offset += 32;
    }

    data[offset..offset + 32].copy_from_slice(config_hash.as_slice());
    offset += 32;

    data[offset..offset + 32].copy_from_slice(tee_image_hash.as_slice());

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
    let signature =
        signer.sign_hash_sync(&hash).map_err(|e| ProposalError::SigningFailed(e.to_string()))?;

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
    use alloy_primitives::{B256, U256, address, b256};
    use rand_08::rngs::OsRng;

    use super::{
        SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, build_signing_data, sign_proposal_data_sync,
        verify_proposal_signature,
    };
    use crate::{crypto::generate_signer, error::ProposalError};

    fn test_signing_data() -> Vec<u8> {
        build_signing_data(
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            U256::from(100),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            U256::from(999),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            U256::from(1000),
            &[],
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("5555555555555555555555555555555555555555555555555555555555555555"),
        )
    }

    #[test]
    fn test_build_signing_data_length() {
        let data = test_signing_data();
        assert_eq!(data.len(), SIGNING_DATA_BASE_LENGTH);
        assert_eq!(data.len(), 276);
    }

    #[test]
    fn test_build_signing_data_components() {
        let proposer = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let l1_origin_hash =
            b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let l1_origin_number = U256::from(100);
        let prev_output_root =
            b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let starting_l2_block = U256::from(999);
        let output_root = b256!("4444444444444444444444444444444444444444444444444444444444444444");
        let ending_l2_block = U256::from(1000);
        let config_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let tee_image_hash =
            b256!("5555555555555555555555555555555555555555555555555555555555555555");

        let data = build_signing_data(
            proposer,
            l1_origin_hash,
            l1_origin_number,
            prev_output_root,
            starting_l2_block,
            output_root,
            ending_l2_block,
            &[],
            config_hash,
            tee_image_hash,
        );

        let mut off = 0;
        assert_eq!(&data[off..off + 20], proposer.as_slice());
        off += 20;
        assert_eq!(&data[off..off + 32], l1_origin_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &l1_origin_number.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], prev_output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &starting_l2_block.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &ending_l2_block.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], config_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], tee_image_hash.as_slice());
    }

    #[test]
    fn test_build_signing_data_with_intermediate_roots() {
        let proposer = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let intermediate_roots = vec![
            b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ];

        let data = build_signing_data(
            proposer,
            B256::ZERO,
            U256::ZERO,
            B256::ZERO,
            U256::ZERO,
            B256::ZERO,
            U256::ZERO,
            &intermediate_roots,
            B256::ZERO,
            B256::ZERO,
        );

        assert_eq!(data.len(), SIGNING_DATA_BASE_LENGTH + 64);

        let ir_offset = 20 + 6 * 32;
        assert_eq!(&data[ir_offset..ir_offset + 32], intermediate_roots[0].as_slice());
        assert_eq!(&data[ir_offset + 32..ir_offset + 64], intermediate_roots[1].as_slice());
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let mut rng = OsRng;
        let signer = generate_signer(&mut rng).expect("failed to generate signer");

        let data = test_signing_data();
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

        let data = test_signing_data();
        let signature = sign_proposal_data_sync(&signer1, &data).expect("signing failed");

        let wrong_public_key = crate::crypto::public_key_bytes(&signer2);
        let valid = verify_proposal_signature(&wrong_public_key, &data, &signature)
            .expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_verify_wrong_data_fails() {
        let mut rng = OsRng;
        let signer = generate_signer(&mut rng).expect("failed to generate signer");

        let data1 = test_signing_data();

        // Build different signing data (change the proposer address)
        let data2 = build_signing_data(
            address!("0000000000000000000000000000000000000001"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            U256::from(100),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
            U256::from(999),
            b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            U256::from(1000),
            &[],
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("5555555555555555555555555555555555555555555555555555555555555555"),
        );

        let signature = sign_proposal_data_sync(&signer, &data1).expect("signing failed");

        let public_key = crate::crypto::public_key_bytes(&signer);
        let valid = verify_proposal_signature(&public_key, &data2, &signature)
            .expect("verification failed");
        assert!(!valid);
    }

    #[test]
    fn test_invalid_signature_length() {
        let public_key = vec![0x04; 65];
        let data = [0u8; SIGNING_DATA_BASE_LENGTH];
        let short_sig = vec![0u8; 64];

        let result = verify_proposal_signature(&public_key, &data, &short_sig);
        assert!(matches!(result, Err(ProposalError::InvalidSignatureLength(64))));
    }
}
